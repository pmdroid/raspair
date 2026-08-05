//! Realtime (type 96) audio transport. RTP timestamps start at the latency
//! offset, audio is sent to the receiver's UDP data port, and a sync packet on
//! the control port maps the RTP timeline onto our NTP wall clock once a
//! second. This is the NTP-timed path that proved audible against Apple TVs.

use super::encrypt::build_audio_packet;
use super::ntp::ntp_now;
use super::rtp::random_u16;
use anyhow::{anyhow, Context, Result};
use std::net::UdpSocket;
use std::time::Duration;

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
        Ok(())
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
