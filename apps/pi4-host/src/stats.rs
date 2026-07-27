//! Lightweight process stats for GET /api/stats.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Stats {
    started: Instant,
    capture_blocks: AtomicU64,
    sessions_started: AtomicU64,
    sessions_stopped: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            capture_blocks: AtomicU64::new(0),
            sessions_started: AtomicU64::new(0),
            sessions_stopped: AtomicU64::new(0),
        }
    }

    pub fn tick_capture(&self) {
        self.capture_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn session_started(&self) {
        self.sessions_started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn session_stopped(&self) {
        self.sessions_stopped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, rms_dbfs: f64) -> StatsSnapshot {
        StatsSnapshot {
            uptime_s: self.started.elapsed().as_secs(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
            capture_blocks: self.capture_blocks.load(Ordering::Relaxed),
            sessions_started: self.sessions_started.load(Ordering::Relaxed),
            sessions_stopped: self.sessions_stopped.load(Ordering::Relaxed),
            rms_dbfs,
            note: "cpu/heap history from ESP firmware not mirrored yet",
        }
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub uptime_s: u64,
    pub version: String,
    pub platform: String,
    pub capture_blocks: u64,
    pub sessions_started: u64,
    pub sessions_stopped: u64,
    pub rms_dbfs: f64,
    pub note: &'static str,
}
