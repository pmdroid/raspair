//! Per-recording sidecar metadata (artist/title/album) next to WAV files.
//!
//! Each recording `foo.wav` may have `foo.json` with tags from music ID or manual edit.

use crate::record::{RecordFile, Recorder};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackMeta {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    /// untagged | identified | manual | skipped | error
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub mbid: Option<String>,
    #[serde(default)]
    pub acoustid: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub duration_sec: Option<f64>,
}

fn default_status() -> String {
    "untagged".into()
}

#[derive(Debug, Clone, Serialize)]
pub struct CollectionEntry {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub status: String,
    pub source: Option<String>,
    pub note: Option<String>,
}

pub fn sidecar_path(dir: &Path, wav_name: &str) -> PathBuf {
    let stem = Path::new(wav_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| wav_name.to_string());
    dir.join(format!("{stem}.json"))
}

pub fn load_meta(dir: &Path, wav_name: &str) -> Option<TrackMeta> {
    let path = sidecar_path(dir, wav_name);
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_meta(dir: &Path, meta: &TrackMeta) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = sidecar_path(dir, &meta.file);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(meta).context("serialize meta")?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn mark_skipped(dir: &Path, wav_name: &str, reason: &str) -> Result<TrackMeta> {
    let mut meta = load_meta(dir, wav_name).unwrap_or_else(|| TrackMeta {
        file: wav_name.to_string(),
        ..Default::default()
    });
    meta.file = wav_name.to_string();
    meta.status = "skipped".into();
    meta.note = Some(reason.to_string());
    meta.updated_at = now_secs();
    save_meta(dir, &meta)?;
    Ok(meta)
}

pub fn list_collection(recorder: &Recorder) -> Result<Vec<CollectionEntry>> {
    let files = recorder.list_files()?;
    let dir = recorder.record_dir();
    Ok(files
        .into_iter()
        .map(|f: RecordFile| {
            let meta = load_meta(dir, &f.name);
            CollectionEntry {
                name: f.name,
                size: f.size,
                mtime: f.mtime,
                artist: meta.as_ref().and_then(|m| m.artist.clone()),
                title: meta.as_ref().and_then(|m| m.title.clone()),
                album: meta.as_ref().and_then(|m| m.album.clone()),
                status: meta
                    .as_ref()
                    .map(|m| m.status.clone())
                    .unwrap_or_else(|| "untagged".into()),
                source: meta.as_ref().and_then(|m| m.source.clone()),
                note: meta.as_ref().and_then(|m| m.note.clone()),
            }
        })
        .collect())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
