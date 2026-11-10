//! Buffered (type 103) audio transport over TCP. Each packet is length-framed
//! (`u16 BE total-length || packet`). The codec is carried in the SSRC high
//! byte — the receiver selects its decoder per packet from that value, so a
//! random SSRC is decoded as garbage.

use super::encrypt::{build_buffered_audio_packet, build_buffered_audio_packet_aes};
use super::rtp::random_u16;
use crate::audio::{encode_alac_escape_frame, PcmEncoder};
use crate::client::{AudioCodec, BufferedAudioFormat};
use crate::session::SessionError;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::io::{Error as IoError, ErrorKind, Write};
use std::net::TcpStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// Codec index (audioFormat trailing-zero bit) shifted into the SSRC high byte.
pub const SSRC_BUFFERED_PCM48000_24_STEREO: u32 = 0x1100_0000;
pub const SSRC_BUFFERED_AAC44100_STEREO: u32 = 0x1600_0000;
pub const SSRC_BUFFERED_AAC48000_STEREO: u32 = 0x1700_0000;

const PEER_DEAD_TIMEOUT: Duration = Duration::from_secs(4);
const WRITE_SLICE: Duration = Duration::from_millis(20);
pub const READ_AHEAD: Duration = Duration::from_secs(2);
pub const QUEUED_AUDIO_CAP: usize = 32 << 20;

/// Map a single-bit audioFormat to its SSRC (codec index in the high byte).
pub fn buffered_ssrc_for_audio_format(audio_format: u64) -> Result<u32> {
    if audio_format == 0 || audio_format & (audio_format - 1) != 0 {
        bail!("audio format must be a single bit, got 0x{audio_format:X}");
    }
    let index = audio_format.trailing_zeros();
    Ok(index << 24)
}

pub struct BufferedStream {
    shk: Vec<u8>,
    /// Optional AES-128-CBC IV. When present (and shk is 16 bytes), the stream
    /// uses the legacy FairPlay AES path instead of ChaCha20-Poly1305.
    aes_iv: Option<Vec<u8>>,
    ssrc: u32,
    receiver_addr: String,
    data_port: i64,
    #[allow(dead_code)]
    control_port: i64,
    conn: Option<TcpStream>,
    seq: u32,
    rtp_timestamp: u32,
    nonce_counter: u64,
    samples_per_frame: i32,
    sample_rate: i32,
    started: bool,
    pub sent: u64,
    pub dropped: u64,
    pub stalled: u64,
}

impl BufferedStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shk: &[u8],
        receiver_addr: &str,
        data_port: i64,
        control_port: i64,
        sample_rate: i32,
        samples_per_frame: i32,
        ssrc: u32,
    ) -> Self {
        Self::new_with_optional_aes_iv(shk, None, receiver_addr, data_port, control_port, sample_rate, samples_per_frame, ssrc)
    }

    pub fn new_with_aes_iv(
        shk: &[u8],
        aes_iv: &[u8],
        receiver_addr: &str,
        data_port: i64,
        control_port: i64,
        sample_rate: i32,
        samples_per_frame: i32,
        ssrc: u32,
    ) -> Self {
        Self::new_with_optional_aes_iv(shk, Some(aes_iv.to_vec()), receiver_addr, data_port, control_port, sample_rate, samples_per_frame, ssrc)
    }

    fn new_with_optional_aes_iv(
        shk: &[u8],
        aes_iv: Option<Vec<u8>>,
        receiver_addr: &str,
        data_port: i64,
        control_port: i64,
        sample_rate: i32,
        samples_per_frame: i32,
        ssrc: u32,
    ) -> Self {
        Self {
            shk: shk.to_vec(),
            aes_iv,
            ssrc,
            receiver_addr: receiver_addr.to_string(),
            data_port,
            control_port,
            conn: None,
            seq: random_u16() as u32,
            rtp_timestamp: 0,
            nonce_counter: 0,
            samples_per_frame,
            sample_rate,
            started: false,
            sent: 0,
            dropped: 0,
            stalled: 0,
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub fn seq(&self) -> u32 {
        self.seq
    }
    pub fn rtp_timestamp(&self) -> u32 {
        self.rtp_timestamp
    }
    pub fn samples_per_frame(&self) -> i32 {
        self.samples_per_frame
    }
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    pub fn set_clock(&mut self, seq: u32, rtp_timestamp: u32) {
        self.seq = seq;
        self.rtp_timestamp = rtp_timestamp;
        self.nonce_counter = 0;
    }

    pub fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        let addr = format!("{}:{}", self.receiver_addr, self.data_port);
        let conn = TcpStream::connect(&addr).with_context(|| format!("dial TCP {addr}"))?;
        // Nagle on: coalesce sub-MSS packet tails (matches the Go reference).
        let _ = conn.set_nodelay(false);
        conn.set_write_timeout(Some(WRITE_SLICE)).ok();
        self.conn = Some(conn);
        self.started = true;
        log::debug!("BufferedStream: TCP connected to {addr}");
        Ok(())
    }

    /// Advance the seq/rtp/nonce state and return the length-framed, AEAD-sealed
    /// buffered packet for `payload` **without** writing it. The entertainment-
    /// group path uses this to push the frame over the leader's main connection
    /// instead of a side `dataPort` TCP.
    pub fn build_framed(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let seq = self.seq;
        let ts = self.rtp_timestamp;
        let nonce_counter = self.nonce_counter;
        self.seq = (self.seq + 1) & 0xFF_FFFF;
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(self.samples_per_frame as u32);
        self.nonce_counter += 1;

        let packet = if let Some(ref iv) = self.aes_iv {
            build_buffered_audio_packet_aes(seq, ts, self.ssrc, &self.shk, iv, payload)?
        } else {
            build_buffered_audio_packet(seq, ts, self.ssrc, &self.shk, nonce_counter, payload)?
        };

        let total = (2 + packet.len()) as u16;
        let mut framed = Vec::with_capacity(2 + packet.len());
        framed.extend_from_slice(&total.to_be_bytes());
        framed.extend_from_slice(&packet);
        Ok(framed)
    }

    pub fn write_frame(&mut self, payload: &[u8], _marker: bool) -> Result<()> {
        self.write_frame_cancellable(payload, &|| false)
    }

    pub fn write_frame_cancellable(&mut self, payload: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        if !self.started {
            self.start()?;
        }

        let framed = match self.build_framed(payload) {
            Ok(f) => f,
            Err(e) => {
                self.dropped += 1;
                return Err(e.context("build audio packet"));
            }
        };

        let conn = self.conn.as_mut().ok_or_else(|| anyhow!("buffered stream not started"))?;
        let (mut offset, mut progress) = (0, Instant::now());
        while offset < framed.len() {
            if cancelled() {
                self.dropped += 1;
                return Err(SessionError::Cancelled.into());
            }
            match conn.write(&framed[offset..]) {
                Ok(0) => {
                    self.dropped += 1;
                    return Err(anyhow!(IoError::from(ErrorKind::WriteZero)).context("write TCP"));
                }
                Ok(n) => (offset, progress) = (offset + n, Instant::now()),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                    if progress.elapsed() >= PEER_DEAD_TIMEOUT {
                        self.stalled += 1;
                        self.dropped += 1;
                        return Err(anyhow!(IoError::new(ErrorKind::TimedOut, format!("peer dead (no bytes accepted for {PEER_DEAD_TIMEOUT:?})"))));
                    }
                }
                Err(e) => {
                    self.dropped += 1;
                    return Err(anyhow!(e).context("write TCP"));
                }
            }
        }
        self.sent += 1;
        log::debug!("BufferedStream: wrote frame seq={} ts={} nc={} payload={} bytes", self.seq.wrapping_sub(1), self.rtp_timestamp.wrapping_sub(self.samples_per_frame as u32), self.nonce_counter - 1, payload.len());
        Ok(())
    }

    pub fn packet_interval(&self) -> Duration {
        Duration::from_nanos((self.samples_per_frame as u64 * 1_000_000_000) / self.sample_rate as u64)
    }

    pub fn stop(&mut self) {
        self.started = false;
        self.conn = None;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferedProgress {
    pub packets: u64,
    pub frames: u64,
    pub queued_frames: u64,
    pub queued_bytes: u64,
    pub peak_queued_frames: u64,
}

#[derive(Default)]
struct QueueState {
    samples: VecDeque<f32>,
    finished: bool,
    closed: Option<SessionError>,
    progress: BufferedProgress,
}

struct QueueShared {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
}

impl QueueShared {
    fn update(&self, f: impl FnOnce(&mut QueueState)) {
        f(&mut self.state.lock().unwrap());
        self.ready.notify_all();
    }

    fn occupancy(&self, state: &mut QueueState) {
        let queued = state.samples.len();
        let p = &mut state.progress;
        p.queued_frames = (queued / FRAME_CHANNELS) as u64;
        p.queued_bytes = (queued * size_of::<f32>()) as u64;
        p.peak_queued_frames = p.peak_queued_frames.max(p.queued_frames);
        self.ready.notify_all();
    }
}

pub struct BufferedProducer(Arc<QueueShared>);
pub struct BufferedFeed(Arc<QueueShared>);

pub fn buffered_queue(sample_rate: u32) -> (BufferedProducer, BufferedFeed) {
    let frames = (READ_AHEAD.as_millis() as usize * sample_rate as usize / 1000).max(FRAME_CHANNELS);
    let capacity = (frames * FRAME_CHANNELS).min(QUEUED_AUDIO_CAP / size_of::<f32>());
    let shared = Arc::new(QueueShared { state: Mutex::default(), ready: Condvar::new(), capacity });
    (BufferedProducer(shared.clone()), BufferedFeed(shared))
}

const FRAME_CHANNELS: usize = 2;

impl BufferedProducer {
    pub fn push(&self, mut samples: &[f32]) -> Result<BufferedProgress, SessionError> {
        if !samples.len().is_multiple_of(FRAME_CHANNELS) {
            return Err(SessionError::InvalidInput);
        }
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(error) = &state.closed {
                return Err(error.clone());
            }
            if state.finished {
                return Err(SessionError::InvalidInput);
            }
            let room = self.0.capacity - state.samples.len();
            let n = (room - room % FRAME_CHANNELS).min(samples.len());
            state.samples.extend(&samples[..n]);
            samples = &samples[n..];
            self.0.occupancy(&mut state);
            if samples.is_empty() {
                return Ok(state.progress);
            }
            state = self.0.ready.wait(state).unwrap();
        }
    }

    pub fn progress(&self) -> BufferedProgress {
        self.0.state.lock().unwrap().progress
    }

    pub fn finish(self) {}
}

impl Drop for BufferedProducer {
    fn drop(&mut self) {
        self.0.update(|s| s.finished = true);
    }
}

impl BufferedFeed {
    pub fn progress(&self) -> BufferedProgress {
        self.0.state.lock().unwrap().progress
    }

    pub fn close(&self, error: SessionError) {
        self.0.update(|s| _ = s.closed.get_or_insert(error));
    }

    pub fn fail(&self, error: &anyhow::Error) {
        self.close(error.downcast_ref::<SessionError>().cloned().unwrap_or(SessionError::ConnectionLost));
    }

    fn next(&self, frames: usize, cancelled: &dyn Fn() -> bool) -> Result<Option<Vec<f32>>> {
        let want = (frames * FRAME_CHANNELS).min(self.0.capacity);
        let mut state = self.0.state.lock().unwrap();
        while state.samples.len() < want && !state.finished {
            if cancelled() {
                return Err(SessionError::Cancelled.into());
            }
            state = self.0.ready.wait_timeout(state, Duration::from_millis(5)).unwrap().0;
        }
        if state.samples.is_empty() {
            return Ok(None);
        }
        let n = want.min(state.samples.len());
        let block = state.samples.drain(..n).collect();
        self.0.occupancy(&mut state);
        Ok(Some(block))
    }

    fn sent(&self, frames: usize) -> BufferedProgress {
        let mut state = self.0.state.lock().unwrap();
        state.progress.packets += 1;
        state.progress.frames += frames as u64;
        state.progress
    }
}

impl Drop for BufferedFeed {
    fn drop(&mut self) {
        self.close(SessionError::Cancelled);
    }
}

pub trait BufferedSink {
    fn send_frame(&mut self, payload: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()>;
}

impl BufferedSink for BufferedStream {
    fn send_frame(&mut self, payload: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        self.write_frame_cancellable(payload, cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferedCodec {
    Pcm24 { little_endian: bool },
    Alac24,
}

impl BufferedCodec {
    pub fn of(format: &BufferedAudioFormat) -> Result<Self, SessionError> {
        match (format.codec, format.sample_rate, format.channels, format.bits_per_sample) {
            (AudioCodec::Pcm, 48000, 2, 24) => Ok(Self::Pcm24 { little_endian: format.little_endian }),
            (AudioCodec::Alac, 48000, 2, 24) => Ok(Self::Alac24),
            _ => Err(SessionError::UnsupportedProfile),
        }
    }

    pub fn encode(&self, block: &[f32], samples_per_frame: usize) -> Vec<u8> {
        match *self {
            Self::Pcm24 { little_endian } => {
                let mut padded = block.to_vec();
                padded.resize(samples_per_frame * FRAME_CHANNELS, 0.0);
                PcmEncoder::new(48000, FRAME_CHANNELS as i32, 24, little_endian).encode(&padded)
            }
            Self::Alac24 => encode_alac_escape_frame(block, samples_per_frame),
        }
    }
}

pub struct BufferedWriter {
    pub codec: BufferedCodec,
    pub samples_per_frame: usize,
    pub interval: Duration,
    pub anchor_local: Instant,
    pub send_lead: Duration,
}

impl BufferedWriter {
    pub fn run<S: BufferedSink>(
        &self,
        sinks: &mut [S],
        feed: &BufferedFeed,
        cancelled: &dyn Fn() -> bool,
        on_progress: &mut dyn FnMut(BufferedProgress),
    ) -> Result<BufferedProgress> {
        self.pump(sinks, feed, cancelled, on_progress).inspect_err(|error| feed.fail(error))
    }

    fn pump<S: BufferedSink>(&self, sinks: &mut [S], feed: &BufferedFeed, cancelled: &dyn Fn() -> bool, on_progress: &mut dyn FnMut(BufferedProgress)) -> Result<BufferedProgress> {
        let mut progress = feed.progress();
        while let Some(block) = feed.next(self.samples_per_frame, cancelled)? {
            let payload = self.codec.encode(&block, self.samples_per_frame);
            for sink in sinks.iter_mut() {
                sink.send_frame(&payload, cancelled).with_context(|| format!("write frame {}", progress.packets))?;
            }
            progress = feed.sent(block.len() / FRAME_CHANNELS);
            on_progress(progress);
            let target = self.anchor_local + self.interval * progress.packets as u32;
            if let Some(due) = target.checked_sub(self.send_lead) {
                wait_until(due, cancelled)?;
            }
        }
        Ok(progress)
    }
}

pub fn wait_until(due: Instant, cancelled: &dyn Fn() -> bool) -> Result<()> {
    while let Some(wait) = due.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        if cancelled() {
            return Err(SessionError::Cancelled.into());
        }
        std::thread::sleep(wait.min(Duration::from_millis(2)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::setup;
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEY: [u8; 32] = [9; 32];
    const RATE_FRAMES: u64 = 96_000;

    fn receiver() -> (TcpListener, BufferedStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port() as i64;
        let mut stream = BufferedStream::new(&KEY, "127.0.0.1", port, 0, 48000, 352, SSRC_BUFFERED_PCM48000_24_STEREO);
        stream.start().unwrap();
        (listener, stream)
    }

    fn writer(codec: BufferedCodec) -> BufferedWriter {
        let interval = Duration::from_nanos(352 * 1_000_000_000 / 48000);
        BufferedWriter { codec, samples_per_frame: 352, interval, anchor_local: Instant::now(), send_lead: Duration::from_secs(3600) }
    }

    fn read_packet(sock: &mut TcpStream) -> Option<(u32, u32, Vec<u8>)> {
        let mut len = [0u8; 2];
        sock.read_exact(&mut len).ok()?;
        let mut packet = vec![0u8; u16::from_be_bytes(len) as usize - 2];
        sock.read_exact(&mut packet).unwrap();
        let (ct, tail) = packet[12..].split_at(packet.len() - 20);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(tail);
        let plain = ChaCha20Poly1305::new(Key::from_slice(&KEY))
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad: &packet[4..12] })
            .unwrap();
        let seq = u32::from_be_bytes([0, packet[1], packet[2], packet[3]]);
        Some((seq, u32::from_be_bytes(packet[4..8].try_into().unwrap()), plain))
    }

    fn sign24(v: u32) -> i32 {
        ((v << 8) as i32) >> 8
    }

    fn decode(codec: BufferedCodec, plain: &[u8]) -> Vec<i32> {
        match codec {
            BufferedCodec::Pcm24 { little_endian } => plain
                .as_chunks::<3>()
                .0
                .iter()
                .map(|b| sign24(if little_endian { u32::from_le_bytes([b[0], b[1], b[2], 0]) } else { u32::from_be_bytes([0, b[0], b[1], b[2]]) }))
                .collect(),
            BufferedCodec::Alac24 => {
                let mut bit = 0usize;
                let mut take = |n: usize| {
                    let v = (0..n).fold(0u32, |acc, i| acc << 1 | (plain[(bit + i) / 8] >> (7 - (bit + i) % 8)) as u32 & 1);
                    bit += n;
                    v
                };
                assert_eq!((take(3), take(4), take(12)), (1, 0, 0));
                let flags = take(4);
                assert_eq!(flags & 0b111, 1);
                let count = if flags & 8 != 0 { take(32) } else { 352 };
                let samples = (0..count * 2).map(|_| sign24(take(24))).collect();
                assert_eq!(take(3), 7);
                samples
            }
        }
    }

    fn source(frames: usize) -> Vec<i32> {
        (0..frames as i64).flat_map(|i| [((i * 7919) % (1 << 23)) as i32, -((i * 104_729) % (1 << 23)) as i32 - 1]).collect()
    }

    fn floats(ints: &[i32]) -> Vec<f32> {
        ints.iter().map(|&v| v as f32 / 8_388_608.0).collect()
    }

    #[test]
    fn pcm_and_alac_frames_preserve_24_bit_edges_impulses_and_final_length() {
        assert_eq!(BufferedCodec::of(&BufferedAudioFormat::aac_48000_stereo()), Err(SessionError::UnsupportedProfile));
        let codecs = [
            BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(true)).unwrap(),
            BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(false)).unwrap(),
            BufferedCodec::of(&BufferedAudioFormat::alac_48000_24_stereo()).unwrap(),
        ];
        assert_eq!(codecs[2], BufferedCodec::Alac24);
        let frames = 352 * 3 + 101;
        let mut ints = vec![0i32; frames * 2];
        ints[..8].copy_from_slice(&[8_388_607, -8_388_608, -8_388_608, 8_388_607, 1, -1, 0, 0]);
        ints[2 * 400] = 8_388_607;
        ints[2 * 700 + 1] = -8_388_608;
        ints[2 * frames - 1] = 4_242;
        for codec in codecs {
            let (listener, mut stream) = receiver();
            let (producer, feed) = buffered_queue(48000);
            let samples = floats(&ints);
            let pusher = std::thread::spawn(move || {
                for chunk in samples.chunks(2 * 333) {
                    producer.push(chunk).unwrap();
                }
            });
            let progress = writer(codec).run(std::slice::from_mut(&mut stream), &feed, &|| false, &mut |_| {}).unwrap();
            pusher.join().unwrap();
            assert_eq!((progress.packets, progress.frames, progress.queued_frames), (4, frames as u64, 0));
            drop(stream);
            let mut sock = listener.accept().unwrap().0;
            let (mut decoded, mut last) = (Vec::new(), None::<(u32, u32)>);
            while let Some((seq, ts, plain)) = read_packet(&mut sock) {
                if let Some((s, t)) = last {
                    assert_eq!((seq, ts), ((s + 1) & 0xFF_FFFF, t.wrapping_add(352)));
                }
                last = Some((seq, ts));
                decoded.extend(decode(codec, &plain));
            }
            let mut expected = ints.clone();
            if codec != BufferedCodec::Alac24 {
                expected.resize(352 * 4 * 2, 0);
            }
            assert_eq!(decoded, expected, "{codec:?}");
        }
    }

    #[test]
    fn delayed_setup_and_slow_receiver_apply_backpressure_without_losing_opening_frames() {
        let (listener, mut stream) = receiver();
        let (producer, feed) = buffered_queue(48000);
        let ints = source(48000 * 5 + 17);
        let samples = floats(&ints);
        let pushed = Arc::new(Mutex::new(0usize));
        let counter = pushed.clone();
        let pusher = std::thread::spawn(move || {
            for chunk in samples.chunks(2 * 4800) {
                producer.push(chunk).unwrap();
                *counter.lock().unwrap() += chunk.len() / 2;
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(feed.progress().queued_frames, RATE_FRAMES);
        assert!(*pushed.lock().unwrap() <= RATE_FRAMES as usize);
        let reader = std::thread::spawn(move || {
            let mut sock = listener.accept().unwrap().0;
            let mut decoded = Vec::new();
            while let Some((_, _, plain)) = read_packet(&mut sock) {
                decoded.extend(decode(BufferedCodec::Pcm24 { little_endian: true }, &plain));
                std::thread::sleep(Duration::from_micros(500));
            }
            decoded
        });
        let codec = BufferedCodec::Pcm24 { little_endian: true };
        let mut peak = 0;
        let progress = writer(codec).run(std::slice::from_mut(&mut stream), &feed, &|| false, &mut |p| peak = peak.max(p.queued_frames)).unwrap();
        pusher.join().unwrap();
        drop(stream);
        let decoded = reader.join().unwrap();
        assert!(peak <= RATE_FRAMES && progress.peak_queued_frames == RATE_FRAMES);
        assert_eq!(progress.frames, ints.len() as u64 / 2);
        assert_eq!(&decoded[..ints.len()], &ints[..]);
        assert!(decoded[ints.len()..].iter().all(|&v| v == 0));
    }

    #[test]
    fn long_source_stays_within_caps_and_cancels_while_socket_is_blocked() {
        let (listener, mut stream) = receiver();
        let _sock = listener.accept().unwrap().0;
        let (producer, feed) = buffered_queue(48000);
        let cancelled = AtomicBool::new(false);
        let pusher = std::thread::spawn(move || {
            let chunk = floats(&source(4096));
            for _ in 0..48000 * 600 / 4096 {
                let p = producer.push(&chunk)?;
                assert!(p.queued_frames <= RATE_FRAMES && p.queued_bytes <= QUEUED_AUDIO_CAP as u64);
            }
            Ok(())
        });
        let (result, waited) = std::thread::scope(|scope| {
            let watcher = scope.spawn(|| {
                let mut last = (feed.progress().packets, Instant::now());
                while last.1.elapsed() < Duration::from_millis(300) {
                    std::thread::sleep(Duration::from_millis(10));
                    let packets = feed.progress().packets;
                    if packets != last.0 {
                        last = (packets, Instant::now());
                    }
                }
                cancelled.store(true, Ordering::SeqCst);
                Instant::now()
            });
            let result = writer(BufferedCodec::Alac24).run(std::slice::from_mut(&mut stream), &feed, &|| cancelled.load(Ordering::SeqCst), &mut |_| {});
            (result, watcher.join().unwrap().elapsed())
        });
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<SessionError>(), Some(&SessionError::Cancelled));
        assert!(waited < Duration::from_millis(200), "{waited:?}");
        assert_eq!(pusher.join().unwrap(), Err(SessionError::Cancelled));
        let progress = feed.progress();
        assert!(progress.packets > 0 && progress.frames < 48000 * 600);
        assert_eq!(progress.peak_queued_frames, RATE_FRAMES);
        assert_eq!(stream.sent, progress.packets);
    }

    #[test]
    fn ssrc_codec_index_in_high_byte() {
        // AAC-LC/44100 is bit 22 -> SSRC 0x16000000; AAC/48000 bit 23 -> 0x17.
        assert_eq!(
            buffered_ssrc_for_audio_format(setup::AUDIO_FORMAT_AAC_44100_2).unwrap(),
            SSRC_BUFFERED_AAC44100_STEREO
        );
        assert_eq!(
            buffered_ssrc_for_audio_format(setup::AUDIO_FORMAT_AAC_48000_2).unwrap(),
            SSRC_BUFFERED_AAC48000_STEREO
        );
        assert_eq!(
            buffered_ssrc_for_audio_format(setup::AUDIO_FORMAT_PCM_48000_24_2).unwrap(),
            SSRC_BUFFERED_PCM48000_24_STEREO
        );
        // Multi-bit input is rejected.
        assert!(buffered_ssrc_for_audio_format(0x3).is_err());
    }
}
