//! Speaker target selection and session intent (mirrors firmware control.c).

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_TARGETS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TargetType {
    Airplay,
    Chromecast,
}

impl Default for TargetType {
    fn default() -> Self {
        Self::Airplay
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerTarget {
    pub name: String,
    pub ip: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(rename = "type", default)]
    pub target_type: TargetType,
    #[serde(default)]
    pub model: Option<String>,
    /// AirPlay mDNS `tsid` — non-empty + matching across two targets = stereo pair.
    /// Serialized as `tsid` for ESP/Android API parity (not the Rust field name).
    #[serde(default, rename = "tsid", alias = "tight_sync_id")]
    pub tight_sync_id: String,
}

fn default_port() -> u16 {
    7000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIntent {
    Idle,
    WantPlay,
    WantStop,
}

#[derive(Debug)]
pub struct ControlState {
    targets: RwLock<Vec<SpeakerTarget>>,
    volume_db: RwLock<f32>,
    intent: RwLock<SessionIntent>,
    streaming: RwLock<bool>,
}

impl ControlState {
    pub fn new(volume_db: f32, initial: Vec<SpeakerTarget>) -> Arc<Self> {
        Arc::new(Self {
            targets: RwLock::new(initial),
            volume_db: RwLock::new(volume_db),
            intent: RwLock::new(SessionIntent::Idle),
            streaming: RwLock::new(false),
        })
    }

    pub fn targets(&self) -> Vec<SpeakerTarget> {
        self.targets.read().clone()
    }

    pub fn set_targets(&self, mut speakers: Vec<SpeakerTarget>) {
        speakers.truncate(MAX_TARGETS);
        *self.targets.write() = speakers;
    }

    pub fn volume_db(&self) -> f32 {
        *self.volume_db.read()
    }

    pub fn set_volume_db(&self, db: f32) {
        *self.volume_db.write() = db.clamp(-144.0, 0.0);
    }

    pub fn intent(&self) -> SessionIntent {
        *self.intent.read()
    }

    pub fn request_play(&self) {
        *self.intent.write() = SessionIntent::WantPlay;
    }

    pub fn request_stop(&self) {
        *self.intent.write() = SessionIntent::WantStop;
    }

    pub fn clear_intent(&self) {
        *self.intent.write() = SessionIntent::Idle;
    }

    pub fn streaming(&self) -> bool {
        *self.streaming.read()
    }

    pub fn set_streaming(&self, v: bool) {
        *self.streaming.write() = v;
    }
}
