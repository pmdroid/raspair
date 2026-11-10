//! Realtime (type 96) audio transport. RTP timestamps start at the latency
//! offset, audio is sent to the receiver's UDP data port, and a sync packet on
//! the control port maps the RTP timeline onto our NTP wall clock once a
//! second. This is the NTP-timed path that proved audible against Apple TVs.

use super::encrypt::build_audio_packet;
use super::ntp::ntp_now;
use super::rtp::random_u16;
use crate::audio::convert::OUTPUT_CHANNELS;
use crate::audio::pcm::{PcmEncoder, FRAME_SIZE};
use crate::session::SessionError;
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const HISTORY_PACKETS: usize = 256;

pub struct RealtimeStream {
    shk: Vec<u8>,
    ssrc: u32,
    receiver_addr: String,
    remote_control: u16,
    seq: u16,
    rtp_timestamp: u32,
    latency_frames: u32,
    samples_per_frame: i32,
    sample_rate: i32,
    payload_type: i32,
    start_ntp: u64,
    frames_sent: u64,
    data_sock: Option<UdpSocket>,
    control_sock: UdpSocket,
    started: bool,
    history: VecDeque<(u16, Vec<u8>)>,
}

impl RealtimeStream {
    /// Binds the local control socket immediately so its port can be
    /// advertised in the stream SETUP.
    pub fn new(shk: &[u8], receiver_addr: &str, sample_rate: i32, samples_per_frame: i32, ssrc: u32) -> Result<Self> {
        let control_sock = UdpSocket::bind("0.0.0.0:0").context("bind control UDP")?;
        let latency = 22050 + sample_rate as u32;
        Ok(Self {
            shk: shk.to_vec(),
            ssrc,
            receiver_addr: receiver_addr.to_string(),
            remote_control: 0,
            seq: random_u16(),
            rtp_timestamp: latency,
            latency_frames: latency,
            samples_per_frame,
            sample_rate,
            payload_type: 96,
            start_ntp: 0,
            frames_sent: 0,
            data_sock: None,
            control_sock,
            started: false,
            history: VecDeque::with_capacity(HISTORY_PACKETS),
        })
    }

    pub fn local_control_port(&self) -> u16 {
        self.control_sock.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn set_payload_type(&mut self, pt: i32) {
        self.payload_type = pt;
    }

    pub fn seq(&self) -> u16 {
        self.seq
    }
    pub fn rtp_timestamp(&self) -> u32 {
        self.rtp_timestamp
    }

    /// Connect to the receiver's data and control ports returned by SETUP.
    pub fn start(&mut self, data_port: i64, remote_control_port: i64) -> Result<()> {
        if self.started {
            return Ok(());
        }
        let sock = UdpSocket::bind("0.0.0.0:0").context("bind data UDP")?;
        sock.connect(format!("{}:{}", self.receiver_addr, data_port))
            .context("connect data UDP")?;
        self.data_sock = Some(sock);
        self.remote_control = remote_control_port as u16;
        self.start_ntp = ntp_now();
        self.started = true;
        Ok(())
    }

    pub fn set_clock(&mut self, seq: u16, rtp_timestamp: u32, start_ntp: u64) {
        self.seq = seq;
        self.rtp_timestamp = rtp_timestamp;
        self.start_ntp = start_ntp;
        self.frames_sent = 0;
    }

    /// Encrypt and send one RTP audio packet.
    pub fn write_frame(&mut self, payload: &[u8], marker: bool) -> Result<()> {
        let seq = self.seq;
        let ts = self.rtp_timestamp;
        self.seq = self.seq.wrapping_add(1);
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(self.samples_per_frame as u32);
        self.frames_sent += self.samples_per_frame as u64;

        let packet = build_audio_packet(seq, ts, self.ssrc, &self.shk, marker, payload, self.payload_type)
            .context("build audio packet")?;
        log::debug!("audio: seq={seq} ts={ts} payload={} packet={}", payload.len(), packet.len());
        self.data_sock
            .as_ref()
            .ok_or_else(|| anyhow!("stream not started"))?
            .send(&packet)
            .context("write UDP")?;
        if self.history.len() == HISTORY_PACKETS {
            self.history.pop_front();
        }
        self.history.push_back((seq, packet));
        Ok(())
    }

    pub fn latency(&self) -> Duration {
        Duration::from_nanos(self.latency_frames as u64 * 1_000_000_000 / self.sample_rate as u64)
    }

    pub fn service_control(&mut self) -> Result<(u64, u64)> {
        self.control_sock.set_nonblocking(true).context("control nonblocking")?;
        let result = self.answer_retransmits();
        self.control_sock.set_nonblocking(false).context("control blocking")?;
        result.context("retransmit")
    }

    fn answer_retransmits(&mut self) -> std::io::Result<(u64, u64)> {
        let (mut requested, mut resent) = (0u64, 0u64);
        let mut buf = [0u8; 64];
        loop {
            let n = match self.control_sock.recv_from(&mut buf) {
                Ok((n, _)) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok((requested, resent)),
                Err(e) => return Err(e),
            };
            if n < 8 || buf[1] & 0x7F != 0x55 {
                continue;
            }
            let first = u16::from_be_bytes([buf[4], buf[5]]);
            let count = u16::from_be_bytes([buf[6], buf[7]]).min(HISTORY_PACKETS as u16);
            requested += count as u64;
            for seq in (0..count).map(|i| first.wrapping_add(i)) {
                if let Some((_, packet)) = self.history.iter().find(|(s, _)| *s == seq) {
                    let mut out = Vec::with_capacity(packet.len() + 4);
                    out.extend_from_slice(&[0x80, 0xD6]);
                    out.extend_from_slice(&seq.to_be_bytes());
                    out.extend_from_slice(packet);
                    self.control_sock.send_to(&out, (self.receiver_addr.as_str(), self.remote_control))?;
                    resent += 1;
                }
            }
        }
    }

    /// Send a RAOP sync packet on the control port mapping the next packet's
    /// RTP timestamp onto our wall clock.
    pub fn send_sync(&mut self, first: bool) -> Result<()> {
        if self.remote_control == 0 {
            return Ok(());
        }
        let rtp_ts = self.rtp_timestamp;
        let mut ntp = self.start_ntp + (((self.frames_sent << 16) / self.sample_rate as u64) << 16);
        if first {
            ntp = ntp_now();
        }

        let mut packet = [0u8; 20];
        packet[0] = if first { 0x90 } else { 0x80 };
        packet[1] = 0xD4;
        packet[2..4].copy_from_slice(&0x0007u16.to_be_bytes());
        packet[4..8].copy_from_slice(&(rtp_ts.wrapping_sub(self.latency_frames)).to_be_bytes());
        packet[8..16].copy_from_slice(&ntp.to_be_bytes());
        packet[16..20].copy_from_slice(&rtp_ts.to_be_bytes());

        self.control_sock
            .send_to(&packet, format!("{}:{}", self.receiver_addr, self.remote_control))
            .context("write sync")?;
        Ok(())
    }

    pub fn packet_interval(&self) -> Duration {
        Duration::from_nanos((self.samples_per_frame as u64 * 1_000_000_000) / self.sample_rate as u64)
    }

    pub fn stop(&mut self) {
        self.started = false;
        self.data_sock = None;
    }
}

pub trait RealtimeSink {
    fn send_frame(&mut self, payload: &[u8], marker: bool) -> Result<()>;
    fn send_sync(&mut self) -> Result<()>;
    fn service(&mut self) -> Result<(u64, u64)>;
}

impl RealtimeSink for RealtimeStream {
    fn send_frame(&mut self, payload: &[u8], marker: bool) -> Result<()> {
        self.write_frame(payload, marker)
    }
    fn send_sync(&mut self) -> Result<()> {
        RealtimeStream::send_sync(self, false)
    }
    fn service(&mut self) -> Result<(u64, u64)> {
        self.service_control()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RealtimeProgress {
    pub packets: u64,
    pub frames: u64,
    pub syncs: u64,
    pub retransmit_requested: u64,
    pub retransmitted: u64,
    pub latency: Duration,
}

pub struct RealtimeSender {
    encoder: PcmEncoder,
    sample_rate: u32,
    sync_every: u64,
    pending: Vec<f32>,
    start: Option<Instant>,
    progress: RealtimeProgress,
}

impl RealtimeSender {
    pub fn new(sample_rate: u32, latency: Duration) -> Self {
        Self {
            encoder: PcmEncoder::new(sample_rate as i32, OUTPUT_CHANNELS as i32, 16, false),
            sample_rate,
            sync_every: (sample_rate as u64 / FRAME_SIZE as u64).max(1),
            pending: Vec::with_capacity(FRAME_SIZE * OUTPUT_CHANNELS),
            start: None,
            progress: RealtimeProgress { latency, ..Default::default() },
        }
    }

    pub fn progress(&self) -> RealtimeProgress {
        self.progress
    }

    pub fn push(&mut self, sink: &mut dyn RealtimeSink, samples: &[f32], cancelled: &dyn Fn() -> bool) -> Result<RealtimeProgress> {
        if !samples.len().is_multiple_of(OUTPUT_CHANNELS) {
            return Err(SessionError::InvalidInput.into());
        }
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(samples);
        let (blocks, rest) = pending.as_chunks::<{ FRAME_SIZE * OUTPUT_CHANNELS }>();
        for block in blocks {
            self.send(sink, block, cancelled)?;
        }
        self.pending = rest.to_vec();
        Ok(self.progress)
    }

    pub fn finish(&mut self, sink: &mut dyn RealtimeSink, cancelled: &dyn Fn() -> bool) -> Result<RealtimeProgress> {
        if !self.pending.is_empty() {
            self.pending.resize(FRAME_SIZE * OUTPUT_CHANNELS, 0.0);
            return self.push(sink, &[], cancelled);
        }
        Ok(self.progress)
    }

    fn send(&mut self, sink: &mut dyn RealtimeSink, block: &[f32], cancelled: &dyn Fn() -> bool) -> Result<()> {
        let start = *self.start.get_or_insert_with(Instant::now);
        let due = start + Duration::from_nanos(self.progress.frames * 1_000_000_000 / self.sample_rate as u64);
        loop {
            if cancelled() {
                return Err(SessionError::Cancelled.into());
            }
            match due.checked_duration_since(Instant::now()) {
                Some(wait) if !wait.is_zero() => std::thread::sleep(wait.min(Duration::from_millis(2))),
                _ => break,
            }
        }
        sink.send_frame(&self.encoder.encode(block), self.progress.packets == 0)?;
        self.progress.packets += 1;
        self.progress.frames += FRAME_SIZE as u64;
        if self.progress.packets.is_multiple_of(self.sync_every) {
            sink.send_sync()?;
            self.progress.syncs += 1;
        }
        let (requested, resent) = sink.service()?;
        self.progress.retransmit_requested += requested;
        self.progress.retransmitted += resent;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    const KEY: [u8; 32] = [7; 32];

    fn loopback(rate: u32) -> (RealtimeStream, UdpSocket, UdpSocket) {
        let data = UdpSocket::bind("127.0.0.1:0").unwrap();
        let control = UdpSocket::bind("127.0.0.1:0").unwrap();
        data.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        control.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut stream = RealtimeStream::new(&KEY, "127.0.0.1", rate as i32, 352, 0x1234).unwrap();
        let port = |s: &UdpSocket| s.local_addr().unwrap().port() as i64;
        stream.start(port(&data), port(&control)).unwrap();
        (stream, data, control)
    }

    fn recv(sock: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let n = sock.recv(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn open(packet: &[u8]) -> (bool, u8, u16, u32, Vec<i16>) {
        let body = &packet[12..];
        let (ct, tail) = body.split_at(body.len() - 8);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(tail);
        let plain = ChaCha20Poly1305::new(Key::from_slice(&KEY))
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad: &packet[4..12] })
            .unwrap();
        let samples = plain.as_chunks::<2>().0.iter().map(|b| i16::from_be_bytes(*b)).collect();
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ts = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        (packet[1] & 0x80 != 0, packet[1] & 0x7F, seq, ts, samples)
    }

    fn pcm(frames: usize) -> (Vec<f32>, Vec<i16>) {
        let ints: Vec<i16> = (0..frames * 2).map(|i| ((i * 37) % 65536) as u16 as i16).collect();
        (ints.iter().map(|&v| v as f32 / 32768.0).collect(), ints)
    }

    #[test]
    fn loopback_packets_are_paced_big_endian_l16() {
        for rate in [44100u32, 48000] {
            let (mut stream, data, _control) = loopback(rate);
            let mut sender = RealtimeSender::new(rate, stream.latency());
            let (samples, ints) = pcm(352 * 3 + 176);
            let start = Instant::now();
            sender.push(&mut stream, &samples[..1000], &|| false).unwrap();
            sender.push(&mut stream, &samples[1000..], &|| false).unwrap();
            let progress = sender.finish(&mut stream, &|| false).unwrap();
            let elapsed = start.elapsed();
            let interval = stream.packet_interval();
            assert!(elapsed >= interval * 3 && elapsed < interval * 3 + Duration::from_millis(200), "{elapsed:?}");
            assert_eq!((progress.packets, progress.frames), (4, 352 * 4));
            assert_eq!(progress.latency, Duration::from_nanos((22050 + rate as u64) * 1_000_000_000 / rate as u64));
            let mut expected = ints.clone();
            expected.resize(352 * 4 * 2, 0);
            let (mut prev_seq, mut prev_ts) = (None::<u16>, 0u32);
            for (i, chunk) in expected.chunks(704).enumerate() {
                let (marker, pt, seq, ts, payload) = open(&recv(&data));
                assert_eq!((marker, pt, payload.len()), (i == 0, 96, 704));
                assert_eq!(payload, chunk, "rate {rate} packet {i}");
                if let Some(p) = prev_seq {
                    assert_eq!((seq, ts), (p.wrapping_add(1), prev_ts.wrapping_add(352)));
                }
                (prev_seq, prev_ts) = (Some(seq), ts);
            }
        }
    }

    #[test]
    fn syncs_once_a_second_and_answers_retransmit_requests() {
        let (mut stream, data, control) = loopback(44100);
        let mut sender = RealtimeSender::new(44100, stream.latency());
        let (samples, _) = pcm(352 * 2);
        sender.push(&mut stream, &samples, &|| false).unwrap();
        let _first = recv(&data);
        let second = recv(&data);
        let lost = u16::from_be_bytes([second[2], second[3]]);
        for seq in [lost, lost.wrapping_sub(500)] {
            let mut request = [0x80, 0xD5, 0, 1, 0, 0, 0, 1];
            request[4..6].copy_from_slice(&seq.to_be_bytes());
            control.send_to(&request, ("127.0.0.1", stream.local_control_port())).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        let (silence, _) = pcm(352 * 123);
        let progress = sender.push(&mut stream, &silence, &|| false).unwrap();
        let resent = recv(&control);
        assert_eq!(&resent[..2], &[0x80, 0xD6]);
        assert_eq!(&resent[2..4], &lost.to_be_bytes());
        assert_eq!(&resent[4..], &second[..]);
        assert_eq!((progress.retransmit_requested, progress.retransmitted), (2, 1));
        assert_eq!((progress.packets, progress.syncs), (125, 1));
        let sync = recv(&control);
        assert_eq!((sync[0], sync[1]), (0x80, 0xD4));
        assert_eq!(u32::from_be_bytes(sync[16..20].try_into().unwrap()), stream.rtp_timestamp());
    }

    #[test]
    fn receiver_disappearance_fails_within_a_few_packets() {
        let (mut stream, data, _control) = loopback(48000);
        drop(data);
        let mut sender = RealtimeSender::new(48000, stream.latency());
        let (samples, _) = pcm(352 * 100);
        let start = Instant::now();
        let error = sender.push(&mut stream, &samples, &|| false).unwrap_err();
        assert!(start.elapsed() < Duration::from_millis(500));
        let kind = error.chain().find_map(|c| c.downcast_ref::<std::io::Error>()).map(|e| e.kind());
        assert_eq!(kind, Some(std::io::ErrorKind::ConnectionRefused));
        assert!(sender.progress().packets < 100);
    }
}
