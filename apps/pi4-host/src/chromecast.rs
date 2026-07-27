//! Chromecast sender path (scaffold).
//!
//! Mirrors `chromecast.c` on the ESP host: open a Cast session and point the
//! receiver at an HTTP PCM stream. Not yet implemented on Linux.

use crate::control::SpeakerTarget;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Serialize)]
pub struct ChromecastSessionInfo {
    pub name: String,
    pub ip: String,
    pub active: bool,
    pub note: &'static str,
}

pub struct ChromecastManager {
    active: AtomicBool,
}

impl ChromecastManager {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn start(
        &self,
        targets: &[SpeakerTarget],
        _stream_url: &str,
        volume_db: f32,
    ) -> anyhow::Result<()> {
        if targets.is_empty() {
            anyhow::bail!("no Chromecast targets");
        }
        for t in targets {
            tracing::warn!(
                name = %t.name,
                ip = %t.ip,
                volume_db,
                "chromecast: session start STUB"
            );
        }
        self.active.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            tracing::info!("chromecast: sessions stopped (stub)");
        }
    }

    pub fn info(&self, targets: &[SpeakerTarget]) -> Vec<ChromecastSessionInfo> {
        let active = self.is_active();
        targets
            .iter()
            .filter(|t| matches!(t.target_type, crate::control::TargetType::Chromecast))
            .map(|t| ChromecastSessionInfo {
                name: t.name.clone(),
                ip: t.ip.clone(),
                active,
                note: "stub — Cast v2 sender not ported yet",
            })
            .collect()
    }
}

impl Default for ChromecastManager {
    fn default() -> Self {
        Self::new()
    }
}
