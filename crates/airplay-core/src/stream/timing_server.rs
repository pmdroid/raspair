//! UDP timing responder for the realtime path. The receiver sends RAOP timing
//! requests (RTP type 0x52) to the port we advertise as `timingPort`; without
//! a reply it can never schedule the RTP anchor and playback stays silent.
//!
//! Packet layout (32 bytes, big-endian):
//!   [0] proto  [1] 0xD2 req / 0xD3 resp  [2..3] seq  [4..7] pad
//!   [8..15] reftime  [16..23] recvtime  [24..31] sendtime

use super::ntp::ntp_now;
use anyhow::{Context, Result};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub struct TimingServer {
    port: u16,
    requests: Arc<AtomicI64>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TimingServer {
    pub fn start() -> Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").context("bind timing UDP")?;
        sock.set_read_timeout(Some(std::time::Duration::from_millis(250)))?;
        let port = sock.local_addr()?.port();
        let requests = Arc::new(AtomicI64::new(0));
        let done = Arc::new(AtomicBool::new(false));

        let req_c = requests.clone();
        let done_c = done.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while !done_c.load(Ordering::Relaxed) {
                let (n, from) = match sock.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if n < 32 {
                    continue;
                }
                let now = ntp_now();
                let mut resp = [0u8; 32];
                resp[0] = buf[0];
                resp[1] = 0xD3;
                resp[2..4].copy_from_slice(&0x0007u16.to_be_bytes());
                resp[8..16].copy_from_slice(&buf[24..32]); // reftime = request send time
                resp[16..24].copy_from_slice(&now.to_be_bytes());
                resp[24..32].copy_from_slice(&now.to_be_bytes());
                let _ = sock.send_to(&resp, from);
                req_c.fetch_add(1, Ordering::Relaxed);
            }
        });

        Ok(Self {
            port,
            requests,
            done,
            handle: Some(handle),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn requests(&self) -> i64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TimingServer {
    fn drop(&mut self) {
        self.stop();
    }
}
