//! Music identification via AcoustID (fpcalc + API) with sidecar collection tags.

use crate::collection::{self, TrackMeta};
use crate::settings::{which_fpcalc, RuntimeSettings};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct IdentifyOutcome {
    pub meta: TrackMeta,
    pub method: String,
    pub candidates: usize,
}

/// Fire-and-forget identify after a recording finalizes.
pub fn spawn_identify(dir: PathBuf, name: String, settings: RuntimeSettings) {
    tokio::spawn(async move {
        match identify_file(&dir, &name, &settings).await {
            Ok(r) => tracing::info!(
                file = %name,
                artist = ?r.meta.artist,
                title = ?r.meta.title,
                method = %r.method,
                "music id complete"
            ),
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "music id failed");
                let mut meta = collection::load_meta(&dir, &name).unwrap_or_else(|| TrackMeta {
                    file: name.clone(),
                    ..Default::default()
                });
                meta.file = name;
                meta.status = "error".into();
                meta.note = Some(e.to_string());
                meta.updated_at = now_secs();
                let _ = collection::save_meta(&dir, &meta);
            }
        }
    });
}

pub async fn identify_file(
    dir: &Path,
    name: &str,
    settings: &RuntimeSettings,
) -> Result<IdentifyOutcome> {
    if !settings.music_id_enabled {
        let meta = collection::mark_skipped(dir, name, "music_id_enabled is false")?;
        return Ok(IdentifyOutcome {
            meta,
            method: "skipped".into(),
            candidates: 0,
        });
    }
    if settings.acoustid_api_key.is_empty() {
        let meta = collection::mark_skipped(
            dir,
            name,
            "no AcoustID API key (set acoustid_api_key or ACOUSTID_API_KEY)",
        )?;
        return Ok(IdentifyOutcome {
            meta,
            method: "skipped".into(),
            candidates: 0,
        });
    }

    let path = dir.join(name);
    if !path.exists() {
        bail!("file not found: {name}");
    }

    let (fingerprint, duration) = match run_fpcalc(&path) {
        Ok(v) => v,
        Err(e) => {
            // Degrade gracefully — write sidecar so UI can manual-tag.
            let meta = TrackMeta {
                file: name.to_string(),
                status: "untagged".into(),
                note: Some(format!("fpcalc unavailable: {e}")),
                source: Some("none".into()),
                updated_at: now_secs(),
                size_bytes: path.metadata().ok().map(|m| m.len()),
                ..Default::default()
            };
            collection::save_meta(dir, &meta)?;
            return Ok(IdentifyOutcome {
                meta,
                method: "none".into(),
                candidates: 0,
            });
        }
    };

    let url = format!(
        "https://api.acoustid.org/v2/lookup?client={}&meta=recordings+releasegroups+compress&duration={}&fingerprint={}",
        urlencoding_simple(&settings.acoustid_api_key),
        duration,
        urlencoding_simple(&fingerprint),
    );

    let client = reqwest::Client::builder()
        .user_agent(format!(
            "vinyl-bridge-pi/{} (https://github.com/pmdroid/vinyl-repo)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("acoustid request")?
        .error_for_status()
        .context("acoustid HTTP")?
        .json::<Value>()
        .await
        .context("acoustid json")?;

    if resp.get("status").and_then(|s| s.as_str()) != Some("ok") {
        bail!("acoustid status not ok");
    }

    let mut best: Option<(f64, TrackMeta)> = None;
    let mut candidates = 0usize;
    if let Some(results) = resp.get("results").and_then(|r| r.as_array()) {
        for r in results {
            let score = r.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
            let aid = r
                .get("id")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            if let Some(recs) = r.get("recordings").and_then(|x| x.as_array()) {
                for rec in recs {
                    candidates += 1;
                    let title = rec
                        .get("title")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());
                    let mbid = rec.get("id").and_then(|t| t.as_str()).map(|s| s.to_string());
                    let artist = rec
                        .get("artists")
                        .and_then(|a| a.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|a| a.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                    let album = rec
                        .get("releasegroups")
                        .and_then(|a| a.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|a| a.get("title"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                    let meta = TrackMeta {
                        file: name.to_string(),
                        artist,
                        title,
                        album,
                        year: None,
                        status: "identified".into(),
                        source: Some("acoustid".into()),
                        mbid,
                        acoustid: aid.clone(),
                        note: Some(format!("score={score:.3}")),
                        updated_at: now_secs(),
                        size_bytes: path.metadata().ok().map(|m| m.len()),
                        duration_sec: Some(duration as f64),
                    };
                    match &best {
                        Some((s, _)) if *s >= score => {}
                        _ => best = Some((score, meta)),
                    }
                }
            }
        }
    }

    let meta = if let Some((_, m)) = best {
        collection::save_meta(dir, &m)?;
        m
    } else {
        let m = TrackMeta {
            file: name.to_string(),
            status: "untagged".into(),
            source: Some("acoustid".into()),
            note: Some("no AcoustID match".into()),
            updated_at: now_secs(),
            size_bytes: path.metadata().ok().map(|m| m.len()),
            duration_sec: Some(duration as f64),
            ..Default::default()
        };
        collection::save_meta(dir, &m)?;
        m
    };

    Ok(IdentifyOutcome {
        meta,
        method: "acoustid".into(),
        candidates,
    })
}

/// Apply manual tags into the sidecar (web / Android).
pub fn tag_manual(
    dir: &Path,
    name: &str,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
) -> Result<TrackMeta> {
    let mut meta = collection::load_meta(dir, name).unwrap_or_else(|| TrackMeta {
        file: name.to_string(),
        ..Default::default()
    });
    meta.file = name.to_string();
    if artist.is_some() {
        meta.artist = artist.filter(|s| !s.trim().is_empty());
    }
    if title.is_some() {
        meta.title = title.filter(|s| !s.trim().is_empty());
    }
    if album.is_some() {
        meta.album = album.filter(|s| !s.trim().is_empty());
    }
    meta.status = if meta.artist.is_some() || meta.title.is_some() {
        "manual".into()
    } else {
        "untagged".into()
    };
    meta.source = Some("manual".into());
    meta.updated_at = now_secs();
    collection::save_meta(dir, &meta)?;
    Ok(meta)
}

fn run_fpcalc(path: &Path) -> Result<(String, u32)> {
    let bin = which_fpcalc().unwrap_or_else(|| PathBuf::from("fpcalc"));
    let out = Command::new(&bin)
        .arg("-json")
        .arg(path)
        .output()
        .context("fpcalc not found — install chromaprint / set FPCALC_PATH")?;
    if !out.status.success() {
        bail!("fpcalc failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("fpcalc json")?;
    let fp = v
        .get("fingerprint")
        .and_then(|f| f.as_str())
        .ok_or_else(|| anyhow::anyhow!("no fingerprint"))?
        .to_string();
    let duration = v
        .get("duration")
        .and_then(|d| d.as_f64().or_else(|| d.as_u64().map(|u| u as f64)))
        .unwrap_or(0.0)
        .round() as u32;
    if fp.is_empty() || duration == 0 {
        bail!("empty fingerprint/duration");
    }
    Ok((fp, duration))
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
