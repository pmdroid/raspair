//! Session WAV recording to a local directory (Pi SD card / rootfs).

use crate::config::Config;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct RecordStatus {
    pub recording: bool,
    pub path: Option<String>,
    pub bytes: u64,
    pub free_bytes: u64,
    pub free_mb: u64,
    pub total_mb: u64,
    pub dropped: u64,
    pub error: Option<String>,
    pub mounted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordFile {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

struct ActiveRecording {
    path: PathBuf,
    file: File,
    data_bytes: u64,
    sample_rate: u32,
    channels: u16,
    bits: u16,
}

pub struct Recorder {
    dir: PathBuf,
    min_free: u64,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    inner: Mutex<Option<ActiveRecording>>,
    dropped: Mutex<u64>,
    last_error: Mutex<Option<String>>,
}

impl Recorder {
    pub fn new(cfg: &Config) -> Arc<Self> {
        let _ = fs::create_dir_all(&cfg.record_dir);
        Arc::new(Self {
            dir: cfg.record_dir.clone(),
            min_free: cfg.record_min_free_bytes,
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            bits: cfg.bits,
            inner: Mutex::new(None),
            dropped: Mutex::new(0),
            last_error: Mutex::new(None),
        })
    }

    pub fn status(&self) -> RecordStatus {
        let (free_bytes, total_bytes) = free_space(&self.dir).unwrap_or((0, 0));
        let guard = self.inner.lock();
        let (recording, path, bytes) = match guard.as_ref() {
            Some(a) => (true, Some(a.path.display().to_string()), a.data_bytes),
            None => (false, None, 0),
        };
        RecordStatus {
            recording,
            path,
            bytes,
            free_bytes,
            free_mb: free_bytes / (1024 * 1024),
            total_mb: total_bytes / (1024 * 1024),
            dropped: *self.dropped.lock(),
            error: self.last_error.lock().clone(),
            mounted: self.dir.exists(),
        }
    }

    pub fn start(&self, filename: Option<String>) -> Result<RecordStatus> {
        if self.inner.lock().is_some() {
            bail!("already recording");
        }
        fs::create_dir_all(&self.dir)?;
        let (free, _) = free_space(&self.dir).unwrap_or((0, 0));
        if free < self.min_free {
            let msg = format!(
                "low free space ({} MB)",
                free / (1024 * 1024)
            );
            *self.last_error.lock() = Some(msg.clone());
            bail!(msg);
        }

        let name = filename.unwrap_or_else(|| {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("rec-{ts}.wav")
        });
        let name = sanitize_filename(&name);
        let path = self.dir.join(&name);
        if path.exists() {
            bail!("file already exists: {name}");
        }

        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        write_wav_header(
            &mut file,
            self.sample_rate,
            self.channels,
            self.bits,
            0,
        )?;

        *self.inner.lock() = Some(ActiveRecording {
            path: path.clone(),
            file,
            data_bytes: 0,
            sample_rate: self.sample_rate,
            channels: self.channels,
            bits: self.bits,
        });
        *self.last_error.lock() = None;
        *self.dropped.lock() = 0;
        tracing::info!(path = %path.display(), "recording started");
        Ok(self.status())
    }

    pub fn stop(&self) -> Result<RecordStatus> {
        let mut guard = self.inner.lock();
        let Some(mut active) = guard.take() else {
            bail!("not recording");
        };
        // Patch WAV sizes
        write_wav_header(
            &mut active.file,
            active.sample_rate,
            active.channels,
            active.bits,
            active.data_bytes as u32,
        )?;
        active.file.flush()?;
        tracing::info!(
            path = %active.path.display(),
            bytes = active.data_bytes,
            "recording stopped"
        );
        drop(guard);
        Ok(self.status())
    }

    /// Append interleaved PCM. Safe to call from capture path.
    pub fn write_pcm(&self, pcm: &[u8]) {
        let mut guard = self.inner.lock();
        let Some(active) = guard.as_mut() else {
            return;
        };
        if let Err(e) = active.file.write_all(pcm) {
            *self.dropped.lock() += 1;
            *self.last_error.lock() = Some(e.to_string());
        } else {
            active.data_bytes += pcm.len() as u64;
        }
    }

    pub fn list_files(&self) -> Result<Vec<RecordFile>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for ent in fs::read_dir(&self.dir)? {
            let ent = ent?;
            let meta = ent.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().ends_with(".wav") {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(RecordFile {
                name,
                size: meta.len(),
                mtime,
            });
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        Ok(out)
    }

    pub fn resolve_download(&self, name: &str) -> Result<PathBuf> {
        let name = sanitize_filename(name);
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            bail!("invalid filename");
        }
        let path = self.dir.join(&name);
        if !path.exists() {
            bail!("not found");
        }
        // Ensure path is under record dir
        let canon_dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
        let canon = path.canonicalize().unwrap_or(path.clone());
        if !canon.starts_with(&canon_dir) {
            bail!("invalid path");
        }
        Ok(canon)
    }
}

fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rec.wav".into());
    let mut out = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if !out.to_ascii_lowercase().ends_with(".wav") {
        out.push_str(".wav");
    }
    out
}

fn free_space(path: &Path) -> Result<(u64, u64)> {
    // Prefer path; if missing, parent.
    let probe = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"))
    };
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let c = CString::new(probe.to_string_lossy().as_bytes())?;
        unsafe {
            let mut s: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut s) != 0 {
                bail!("statvfs failed");
            }
            let bsize = s.f_frsize as u64;
            let free = s.f_bavail as u64 * bsize;
            let total = s.f_blocks as u64 * bsize;
            return Ok((free, total));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = probe;
        Ok((0, 0))
    }
}

fn write_wav_header(
    file: &mut File,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    data_bytes: u32,
) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
    let block_align = channels * (bits / 8);
    let riff_size = 36 + data_bytes;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // PCM chunk size
    file.write_all(&1u16.to_le_bytes())?; // PCM format
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}
