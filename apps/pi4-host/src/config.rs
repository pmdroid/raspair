//! Build / runtime configuration for the Pi host.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_device_name")]
    pub device_name: String,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    #[serde(default = "default_mdns_hostname")]
    pub mdns_hostname: String,
    #[serde(default = "default_capture_device")]
    pub capture_device: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    #[serde(default = "default_bits")]
    pub bits: u16,
    #[serde(default = "default_record_dir")]
    pub record_dir: PathBuf,
    #[serde(default = "default_record_min_free")]
    pub record_min_free_bytes: u64,
    #[serde(default = "default_start_threshold")]
    pub audio_start_threshold_db: f64,
    #[serde(default = "default_start_hold")]
    pub audio_start_hold_ms: u64,
    #[serde(default = "default_stop_threshold")]
    pub audio_stop_threshold_db: f64,
    #[serde(default = "default_stop_timeout")]
    pub audio_stop_timeout_sec: u64,
    #[serde(default = "default_volume")]
    pub volume_db: f32,
    #[serde(default)]
    pub force_open: bool,
    /// Auto-start/stop WAV recording when the energy gate opens/closes.
    #[serde(default)]
    pub auto_record: bool,
    /// Attempt AcoustID identification after a recording finalizes.
    #[serde(default)]
    pub music_id_enabled: bool,
    /// AcoustID client API key (also overridable via ACOUSTID_API_KEY).
    #[serde(default)]
    pub acoustid_api_key: String,
    /// Silence-split multi-track mode for record sessions.
    #[serde(default)]
    pub record_auto_split: bool,
    #[serde(default = "default_track_start_threshold")]
    pub track_start_threshold_db: f64,
    #[serde(default = "default_track_start_hold")]
    pub track_start_hold_ms: u64,
    #[serde(default = "default_track_stop_threshold")]
    pub track_stop_threshold_db: f64,
    #[serde(default = "default_track_stop_hold")]
    pub track_stop_hold_ms: u64,
    #[serde(default = "default_track_preroll")]
    pub track_preroll_ms: u64,
    #[serde(default = "default_track_tail")]
    pub track_tail_ms: u64,
    #[serde(default = "default_track_min_sec")]
    pub track_min_sec: f64,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    pub ip: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_airplay_port")]
    pub port: u16,
    /// "airplay" or "chromecast"
    #[serde(rename = "type", default = "default_target_type")]
    pub target_type: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            http_bind: default_http_bind(),
            mdns_hostname: default_mdns_hostname(),
            capture_device: default_capture_device(),
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            bits: default_bits(),
            record_dir: default_record_dir(),
            record_min_free_bytes: default_record_min_free(),
            audio_start_threshold_db: default_start_threshold(),
            audio_start_hold_ms: default_start_hold(),
            audio_stop_threshold_db: default_stop_threshold(),
            audio_stop_timeout_sec: default_stop_timeout(),
            volume_db: default_volume(),
            force_open: false,
            auto_record: false,
            music_id_enabled: false,
            acoustid_api_key: String::new(),
            record_auto_split: false,
            track_start_threshold_db: default_track_start_threshold(),
            track_start_hold_ms: default_track_start_hold(),
            track_stop_threshold_db: default_track_stop_threshold(),
            track_stop_hold_ms: default_track_stop_hold(),
            track_preroll_ms: default_track_preroll(),
            track_tail_ms: default_track_tail(),
            track_min_sec: default_track_min_sec(),
            targets: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            tracing::info!("no config file; using defaults");
            return Ok(Self::default());
        };
        if !path.exists() {
            tracing::warn!(
                path = %path.display(),
                "config file missing; using defaults"
            );
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

}

fn default_device_name() -> String {
    "Vinyl Bridge".into()
}
fn default_http_bind() -> String {
    "0.0.0.0:80".into()
}
fn default_mdns_hostname() -> String {
    "vinyl-bridge".into()
}
fn default_capture_device() -> String {
    "default".into()
}
fn default_sample_rate() -> u32 {
    48_000
}
fn default_channels() -> u16 {
    2
}
fn default_bits() -> u16 {
    24
}
fn default_record_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("./rec")
    } else {
        PathBuf::from("/var/lib/vinyl-bridge/rec")
    }
}
fn default_record_min_free() -> u64 {
    16 * 1024 * 1024
}
fn default_start_threshold() -> f64 {
    -45.0
}
fn default_start_hold() -> u64 {
    250
}
fn default_stop_threshold() -> f64 {
    -52.0
}
fn default_stop_timeout() -> u64 {
    15
}
fn default_volume() -> f32 {
    -30.0
}
fn default_airplay_port() -> u16 {
    7000
}
fn default_target_type() -> String {
    "airplay".into()
}
fn default_track_start_threshold() -> f64 {
    -45.0
}
fn default_track_start_hold() -> u64 {
    300
}
fn default_track_stop_threshold() -> f64 {
    -55.0
}
fn default_track_stop_hold() -> u64 {
    2500
}
fn default_track_preroll() -> u64 {
    400
}
fn default_track_tail() -> u64 {
    300
}
fn default_track_min_sec() -> f64 {
    2.0
}
