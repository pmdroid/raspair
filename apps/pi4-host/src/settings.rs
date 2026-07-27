//! Runtime settings store (HTTP GET/PUT /api/settings).
//!
//! Seeded from TOML config; optional JSON persist next to record_dir.

use crate::config::Config;
use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Live snapshot returned by GET /api/settings and used by the session gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub device_name: String,
    pub volume_db: f32,
    pub force_open: bool,
    pub auto_record: bool,
    pub music_id_enabled: bool,
    /// True when an AcoustID API key is configured and fpcalc is available.
    pub music_id_ready: bool,
    pub fpcalc_available: bool,
    pub record_dir: String,
    pub audio_start_threshold_db: f64,
    pub audio_start_hold_ms: u64,
    pub audio_stop_threshold_db: f64,
    pub audio_stop_timeout_sec: u64,
    pub record_auto_split: bool,
    pub track_start_threshold_db: f64,
    pub track_start_hold_ms: u64,
    pub track_stop_threshold_db: f64,
    pub track_stop_hold_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub capture_device: String,
    /// Never serialized over HTTP (use `acoustid_configured`).
    #[serde(skip)]
    pub acoustid_api_key: String,
    pub acoustid_configured: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsPatch {
    pub device_name: Option<String>,
    pub volume_db: Option<f32>,
    pub force_open: Option<bool>,
    pub auto_record: Option<bool>,
    pub music_id_enabled: Option<bool>,
    pub acoustid_api_key: Option<String>,
    pub audio_start_threshold_db: Option<f64>,
    pub audio_start_hold_ms: Option<u64>,
    pub audio_stop_threshold_db: Option<f64>,
    pub audio_stop_timeout_sec: Option<u64>,
    pub record_auto_split: Option<bool>,
    pub track_start_threshold_db: Option<f64>,
    pub track_start_hold_ms: Option<u64>,
    pub track_stop_threshold_db: Option<f64>,
    pub track_stop_hold_ms: Option<u64>,
}

pub struct SettingsStore {
    path: PathBuf,
    inner: RwLock<RuntimeSettings>,
}

impl SettingsStore {
    pub fn new(cfg: &Config) -> Arc<Self> {
        let path = settings_path(&cfg.record_dir);
        let mut s = from_config(cfg);
        if let Ok(disk) = load_disk(&path) {
            apply_disk(&mut s, &disk);
        }
        refresh_caps(&mut s);
        Arc::new(Self {
            path,
            inner: RwLock::new(s),
        })
    }

    pub fn get(&self) -> RuntimeSettings {
        let mut s = self.inner.read().clone();
        refresh_caps(&mut s);
        s
    }

    pub fn snapshot(&self) -> RuntimeSettings {
        self.inner.read().clone()
    }

    pub fn patch(&self, p: SettingsPatch) -> Result<RuntimeSettings> {
        {
            let mut s = self.inner.write();
            if let Some(v) = p.device_name {
                if !v.trim().is_empty() {
                    s.device_name = v.trim().to_string();
                }
            }
            if let Some(v) = p.volume_db {
                s.volume_db = v.clamp(-144.0, 0.0);
            }
            if let Some(v) = p.force_open {
                s.force_open = v;
            }
            if let Some(v) = p.auto_record {
                s.auto_record = v;
            }
            if let Some(v) = p.music_id_enabled {
                s.music_id_enabled = v;
            }
            if let Some(v) = p.acoustid_api_key {
                s.acoustid_api_key = v.trim().to_string();
            }
            if let Some(v) = p.audio_start_threshold_db {
                s.audio_start_threshold_db = v.clamp(-120.0, 0.0);
            }
            if let Some(v) = p.audio_start_hold_ms {
                s.audio_start_hold_ms = v.clamp(50, 10_000);
            }
            if let Some(v) = p.audio_stop_threshold_db {
                s.audio_stop_threshold_db = v.clamp(-120.0, 0.0);
            }
            if let Some(v) = p.audio_stop_timeout_sec {
                s.audio_stop_timeout_sec = v.clamp(1, 600);
            }
            if let Some(v) = p.record_auto_split {
                s.record_auto_split = v;
            }
            if let Some(v) = p.track_start_threshold_db {
                s.track_start_threshold_db = v;
            }
            if let Some(v) = p.track_start_hold_ms {
                s.track_start_hold_ms = v;
            }
            if let Some(v) = p.track_stop_threshold_db {
                s.track_stop_threshold_db = v;
            }
            if let Some(v) = p.track_stop_hold_ms {
                s.track_stop_hold_ms = v;
            }
            refresh_caps(&mut s);
        }
        self.persist()?;
        Ok(self.get())
    }

    fn persist(&self) -> Result<()> {
        let s = self.inner.read().clone();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let disk = DiskSettings {
            device_name: Some(s.device_name),
            volume_db: Some(s.volume_db),
            force_open: Some(s.force_open),
            auto_record: Some(s.auto_record),
            music_id_enabled: Some(s.music_id_enabled),
            acoustid_api_key: Some(s.acoustid_api_key),
            audio_start_threshold_db: Some(s.audio_start_threshold_db),
            audio_start_hold_ms: Some(s.audio_start_hold_ms),
            audio_stop_threshold_db: Some(s.audio_stop_threshold_db),
            audio_stop_timeout_sec: Some(s.audio_stop_timeout_sec),
            record_auto_split: Some(s.record_auto_split),
            track_start_threshold_db: Some(s.track_start_threshold_db),
            track_start_hold_ms: Some(s.track_start_hold_ms),
            track_stop_threshold_db: Some(s.track_stop_threshold_db),
            track_stop_hold_ms: Some(s.track_stop_hold_ms),
        };
        let text = serde_json::to_string_pretty(&disk)?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("write {}", self.path.display()))?;
        tracing::info!(path = %self.path.display(), "settings saved");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DiskSettings {
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    volume_db: Option<f32>,
    #[serde(default)]
    force_open: Option<bool>,
    #[serde(default)]
    auto_record: Option<bool>,
    #[serde(default)]
    music_id_enabled: Option<bool>,
    #[serde(default)]
    acoustid_api_key: Option<String>,
    #[serde(default)]
    audio_start_threshold_db: Option<f64>,
    #[serde(default)]
    audio_start_hold_ms: Option<u64>,
    #[serde(default)]
    audio_stop_threshold_db: Option<f64>,
    #[serde(default)]
    audio_stop_timeout_sec: Option<u64>,
    #[serde(default)]
    record_auto_split: Option<bool>,
    #[serde(default)]
    track_start_threshold_db: Option<f64>,
    #[serde(default)]
    track_start_hold_ms: Option<u64>,
    #[serde(default)]
    track_stop_threshold_db: Option<f64>,
    #[serde(default)]
    track_stop_hold_ms: Option<u64>,
}

fn settings_path(record_dir: &Path) -> PathBuf {
    record_dir
        .parent()
        .map(|p| p.join("settings.json"))
        .unwrap_or_else(|| PathBuf::from("settings.json"))
}

fn load_disk(path: &Path) -> Result<DiskSettings> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn apply_disk(s: &mut RuntimeSettings, d: &DiskSettings) {
    if let Some(ref v) = d.device_name {
        if !v.trim().is_empty() {
            s.device_name = v.clone();
        }
    }
    if let Some(v) = d.volume_db {
        s.volume_db = v;
    }
    if let Some(v) = d.force_open {
        s.force_open = v;
    }
    if let Some(v) = d.auto_record {
        s.auto_record = v;
    }
    if let Some(v) = d.music_id_enabled {
        s.music_id_enabled = v;
    }
    if let Some(ref v) = d.acoustid_api_key {
        s.acoustid_api_key = v.clone();
    }
    if let Some(v) = d.audio_start_threshold_db {
        s.audio_start_threshold_db = v;
    }
    if let Some(v) = d.audio_start_hold_ms {
        s.audio_start_hold_ms = v;
    }
    if let Some(v) = d.audio_stop_threshold_db {
        s.audio_stop_threshold_db = v;
    }
    if let Some(v) = d.audio_stop_timeout_sec {
        s.audio_stop_timeout_sec = v;
    }
    if let Some(v) = d.record_auto_split {
        s.record_auto_split = v;
    }
    if let Some(v) = d.track_start_threshold_db {
        s.track_start_threshold_db = v;
    }
    if let Some(v) = d.track_start_hold_ms {
        s.track_start_hold_ms = v;
    }
    if let Some(v) = d.track_stop_threshold_db {
        s.track_stop_threshold_db = v;
    }
    if let Some(v) = d.track_stop_hold_ms {
        s.track_stop_hold_ms = v;
    }
}

fn from_config(cfg: &Config) -> RuntimeSettings {
    let mut key = cfg.acoustid_api_key.clone();
    if key.is_empty() {
        if let Ok(k) = std::env::var("ACOUSTID_API_KEY") {
            key = k;
        }
    }
    let mut s = RuntimeSettings {
        device_name: cfg.device_name.clone(),
        volume_db: cfg.volume_db,
        force_open: cfg.force_open,
        auto_record: cfg.auto_record,
        music_id_enabled: cfg.music_id_enabled,
        music_id_ready: false,
        fpcalc_available: false,
        record_dir: cfg.record_dir.display().to_string(),
        audio_start_threshold_db: cfg.audio_start_threshold_db,
        audio_start_hold_ms: cfg.audio_start_hold_ms,
        audio_stop_threshold_db: cfg.audio_stop_threshold_db,
        audio_stop_timeout_sec: cfg.audio_stop_timeout_sec,
        record_auto_split: cfg.record_auto_split,
        track_start_threshold_db: cfg.track_start_threshold_db,
        track_start_hold_ms: cfg.track_start_hold_ms,
        track_stop_threshold_db: cfg.track_stop_threshold_db,
        track_stop_hold_ms: cfg.track_stop_hold_ms,
        sample_rate: cfg.sample_rate,
        channels: cfg.channels,
        bits: cfg.bits,
        capture_device: cfg.capture_device.clone(),
        acoustid_api_key: key,
        acoustid_configured: false,
    };
    refresh_caps(&mut s);
    s
}

fn refresh_caps(s: &mut RuntimeSettings) {
    s.fpcalc_available = which_fpcalc().is_some();
    s.acoustid_configured = !s.acoustid_api_key.trim().is_empty();
    s.music_id_ready =
        s.music_id_enabled && s.acoustid_configured && s.fpcalc_available;
}

/// Return path to `fpcalc` if found.
pub fn which_fpcalc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FPCALC_PATH") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    for cand in [
        "/usr/bin/fpcalc",
        "/usr/local/bin/fpcalc",
        "/opt/homebrew/bin/fpcalc",
    ] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = PathBuf::from(dir).join("fpcalc");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
