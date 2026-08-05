//! Buffered (type 103) audio transport over TCP. Each packet is length-framed
//! (`u16 BE total-length || packet`). The codec is carried in the SSRC high
//! byte — the receiver selects its decoder per packet from that value, so a
//! random SSRC is decoded as garbage.

use super::encrypt::{build_buffered_audio_packet, build_buffered_audio_packet_aes};
use super::rtp::random_u16;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

// Codec index (audioFormat trailing-zero bit) shifted into the SSRC high byte.
pub const SSRC_BUFFERED_PCM48000_24_STEREO: u32 = 0x1100_0000;
pub const SSRC_BUFFERED_AAC44100_STEREO: u32 = 0x1600_0000;
pub const SSRC_BUFFERED_AAC48000_STEREO: u32 = 0x1700_0000;

const PEER_DEAD_TIMEOUT: Duration = Duration::from_secs(4);

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
        conn.set_write_timeout(Some(PEER_DEAD_TIMEOUT)).ok();
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
        match conn.write_all(&framed) {
            Ok(()) => {
                self.sent += 1;
                log::debug!("BufferedStream: wrote frame seq={} ts={} nc={} payload={} bytes", self.seq - 1, self.rtp_timestamp - self.samples_per_frame as u32, self.nonce_counter - 1, payload.len());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                self.stalled += 1;
                self.dropped += 1;
                Err(anyhow!("peer dead (no bytes accepted for {PEER_DEAD_TIMEOUT:?}): {e}"))
            }
            Err(e) => {
                self.dropped += 1;
                Err(anyhow!("write TCP: {e}"))
            }
        }
    }

    pub fn packet_interval(&self) -> Duration {
        Duration::from_nanos((self.samples_per_frame as u64 * 1_000_000_000) / self.sample_rate as u64)
    }

    pub fn stop(&mut self) {
        self.started = false;
        self.conn = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::setup;

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
