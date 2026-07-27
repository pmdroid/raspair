//! AirPlay 2 session orchestrator (scaffold).
//!
//! Full parity with `apps/firmware/components/airplay2` is not yet ported.
//! This module owns session lifecycle hooks so the control loop and HTTP API
//! match the ESP host shape; streaming/crypto/RTSP are stubs.

use crate::control::SpeakerTarget;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Serialize)]
pub struct AirplaySessionInfo {
    pub name: String,
    pub ip: String,
    pub active: bool,
    pub note: &'static str,
}

pub struct AirplayManager {
    active: AtomicBool,
}

impl AirplayManager {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Start AirPlay sessions toward the given targets.
    ///
    /// **Parity gap**: real RTSP / HAP pair-verify / ALAC / buffered TCP send
    /// lives in `apps/firmware/components/airplay2`. Port or FFI that library
    /// (or a Rust reimplementation) before claiming drop-free HomePod audio.
    pub fn start(&self, targets: &[SpeakerTarget], volume_db: f32) -> anyhow::Result<()> {
        if targets.is_empty() {
            anyhow::bail!("no AirPlay targets");
        }
        for t in targets {
            tracing::warn!(
                name = %t.name,
                ip = %t.ip,
                port = t.port,
                volume_db,
                "airplay: session start STUB (protocol not yet ported)"
            );
        }
        self.active.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            tracing::info!("airplay: sessions stopped (stub)");
        }
    }

    pub fn set_volume(&self, volume_db: f32) {
        if self.is_active() {
            tracing::debug!(volume_db, "airplay: volume STUB");
        }
    }

    pub fn info(&self, targets: &[SpeakerTarget]) -> Vec<AirplaySessionInfo> {
        let active = self.is_active();
        targets
            .iter()
            .filter(|t| matches!(t.target_type, crate::control::TargetType::Airplay))
            .map(|t| AirplaySessionInfo {
                name: t.name.clone(),
                ip: t.ip.clone(),
                active,
                note: "stub — airplay2 protocol not ported yet",
            })
            .collect()
    }
}

impl Default for AirplayManager {
    fn default() -> Self {
        Self::new()
    }
}
