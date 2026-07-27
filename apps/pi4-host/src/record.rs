//! Session WAV recording to a local directory (Pi SD card / rootfs).
//!
//! Supports continuous session files and silence-gated multi-track auto-split
//! (vinyl inter-track gaps → one WAV per song).

use crate::config::Config;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
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
    /// Alias for Android clients that expect `dropped_bytes`.
    pub dropped_bytes: u64,
    /// Approximate seconds from PCM size (session mode).
    pub seconds: u64,
    pub error: Option<String>,
    pub mounted: bool,
    /// Echo of settings.auto_record (filled by webapi).
    #[serde(default)]
    pub auto_record: bool,
    /// True when silence auto-split is active for this session.
    pub auto_split: bool,
    /// Phase: idle | listening | recording_track | recording_session
    pub phase: String,
    /// Tracks finalized in the current armed session (auto-split).
    pub tracks_this_session: u32,
    pub level_dbfs: f64,
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
    loud_end_bytes: u64,
}

enum SplitPhase {
    Idle,
    Listening,
    RecordingTrack,
    RecordingSession,
}

struct SplitEngine {
    phase: SplitPhase,
    auto_split: bool,
    loud_ms: f64,
    quiet_ms: f64,
    preroll: VecDeque<Vec<u8>>,
    preroll_bytes: usize,
    track_index: u32,
    level_dbfs: f64,
    session_prefix: String,
}

pub struct Recorder {
    dir: PathBuf,
    min_free: u64,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    start_db: f64,
    start_hold_ms: f64,
    stop_db: f64,
    stop_hold_ms: f64,
    preroll_ms: f64,
    tail_ms: f64,
    min_track_sec: f64,
    default_auto_split: bool,
    inner: Mutex<Option<ActiveRecording>>,
    split: Mutex<SplitEngine>,
    dropped: Mutex<u64>,
    last_error: Mutex<Option<String>>,
    last_finished: Mutex<Option<String>>,
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
            start_db: cfg.track_start_threshold_db,
            start_hold_ms: cfg.track_start_hold_ms as f64,
            stop_db: cfg.track_stop_threshold_db,
            stop_hold_ms: cfg.track_stop_hold_ms as f64,
            preroll_ms: cfg.track_preroll_ms as f64,
            tail_ms: cfg.track_tail_ms as f64,
            min_track_sec: cfg.track_min_sec,
            default_auto_split: cfg.record_auto_split,
            inner: Mutex::new(None),
            split: Mutex::new(SplitEngine {
                phase: SplitPhase::Idle,
                auto_split: false,
                loud_ms: 0.0,
                quiet_ms: 0.0,
                preroll: VecDeque::new(),
                preroll_bytes: 0,
                track_index: 0,
                level_dbfs: -120.0,
                session_prefix: String::new(),
            }),
            dropped: Mutex::new(0),
            last_error: Mutex::new(None),
            last_finished: Mutex::new(None),
        })
    }

    pub fn is_recording(&self) -> bool {
        let split = self.split.lock();
        !matches!(split.phase, SplitPhase::Idle) || self.inner.lock().is_some()
    }

    pub fn peek_last_finished(&self) -> Option<String> {
        self.last_finished.lock().clone()
    }

    pub fn take_last_finished(&self) -> Option<String> {
        self.last_finished.lock().take()
    }

    pub fn status(&self) -> RecordStatus {
        let (free_bytes, total_bytes) = free_space(&self.dir).unwrap_or((0, 0));
        let guard = self.inner.lock();
        let (recording_file, path, bytes) = match guard.as_ref() {
            Some(a) => (true, Some(a.path.display().to_string()), a.data_bytes),
            None => (false, None, 0),
        };
        let split = self.split.lock();
        let phase = match split.phase {
            SplitPhase::Idle => "idle",
            SplitPhase::Listening => "listening",
            SplitPhase::RecordingTrack => "recording_track",
            SplitPhase::RecordingSession => "recording_session",
        };
        let armed = !matches!(split.phase, SplitPhase::Idle);
        let dropped = *self.dropped.lock();
        let bps = (self.sample_rate as u64)
            * (self.channels as u64)
            * (self.bits as u64 / 8).max(1);
        let seconds = if bps > 0 { bytes / bps } else { 0 };
        RecordStatus {
            recording: armed || recording_file,
            path,
            bytes,
            free_bytes,
            free_mb: free_bytes / (1024 * 1024),
            total_mb: total_bytes / (1024 * 1024),
            dropped,
            dropped_bytes: dropped,
            seconds,
            error: self.last_error.lock().clone(),
            mounted: self.dir.exists(),
            auto_record: false,
            auto_split: split.auto_split,
            phase: phase.into(),
            tracks_this_session: split.track_index,
            level_dbfs: split.level_dbfs,
        }
    }

    pub fn start(&self, filename: Option<String>) -> Result<RecordStatus> {
        {
            let split = self.split.lock();
            if !matches!(split.phase, SplitPhase::Idle) || self.inner.lock().is_some() {
                bail!("already recording");
            }
        }
        fs::create_dir_all(&self.dir)?;
        let (free, _) = free_space(&self.dir).unwrap_or((0, 0));
        if free < self.min_free {
            let msg = format!("low free space ({} MB)", free / (1024 * 1024));
            *self.last_error.lock() = Some(msg.clone());
            bail!(msg);
        }

        let use_split = self.default_auto_split && filename.is_none();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut split = self.split.lock();
        split.auto_split = use_split;
        split.loud_ms = 0.0;
        split.quiet_ms = 0.0;
        split.preroll.clear();
        split.preroll_bytes = 0;
        split.track_index = 0;
        split.level_dbfs = -120.0;
        split.session_prefix = format!("rec-{ts}");

        if use_split {
            split.phase = SplitPhase::Listening;
            *self.last_error.lock() = None;
            *self.dropped.lock() = 0;
            tracing::info!("recording armed (auto-split / silence track boundaries)");
            drop(split);
            return Ok(self.status());
        }

        let name = filename.unwrap_or_else(|| format!("rec-{ts}.wav"));
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
            loud_end_bytes: 0,
        });
        split.phase = SplitPhase::RecordingSession;
        *self.last_error.lock() = None;
        *self.dropped.lock() = 0;
        tracing::info!(path = %path.display(), "recording started (session)");
        drop(split);
        Ok(self.status())
    }

    pub fn stop(&self) -> Result<RecordStatus> {
        if let Some(active) = self.inner.lock().take() {
            let name = active
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
            let fin = finalize_active(active, self.tail_bytes(), false)?;
            if let Some(n) = name.or_else(|| {
                fin.path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            }) {
                *self.last_finished.lock() = Some(n);
            }
        }

        let mut split = self.split.lock();
        split.phase = SplitPhase::Idle;
        split.auto_split = false;
        split.preroll.clear();
        split.preroll_bytes = 0;
        split.loud_ms = 0.0;
        split.quiet_ms = 0.0;
        drop(split);

        tracing::info!("recording stopped");
        Ok(self.status())
    }

    /// Append interleaved PCM. Safe to call from capture path.
    pub fn write_pcm(&self, pcm: &[u8]) {
        let frame = frame_bytes(self.channels, self.bits);
        if frame == 0 || pcm.is_empty() {
            return;
        }
        let usable = pcm.len() - (pcm.len() % frame);
        if usable == 0 {
            return;
        }
        let block = &pcm[..usable];
        let db = block_dbfs(block, self.bits);
        let block_ms = (usable / frame) as f64 * 1000.0 / self.sample_rate as f64;

        let mut split = self.split.lock();
        split.level_dbfs = db;

        match split.phase {
            SplitPhase::Idle => {}
            SplitPhase::RecordingSession => {
                drop(split);
                self.append_pcm(block);
            }
            SplitPhase::Listening => {
                let preroll_cap = (self.preroll_ms / 1000.0
                    * self.sample_rate as f64
                    * frame as f64) as usize;
                split.preroll.push_back(block.to_vec());
                split.preroll_bytes += usable;
                while split.preroll_bytes > preroll_cap && split.preroll.len() > 1 {
                    if let Some(old) = split.preroll.pop_front() {
                        split.preroll_bytes = split.preroll_bytes.saturating_sub(old.len());
                    }
                }
                if db > self.start_db {
                    split.loud_ms += block_ms;
                    if split.loud_ms >= self.start_hold_ms {
                        let prefix = split.session_prefix.clone();
                        let idx = split.track_index + 1;
                        let preroll: Vec<Vec<u8>> = split.preroll.drain(..).collect();
                        split.preroll_bytes = 0;
                        split.track_index = idx;
                        split.loud_ms = 0.0;
                        split.quiet_ms = 0.0;
                        split.phase = SplitPhase::RecordingTrack;
                        drop(split);
                        if let Err(e) = self.open_track_file(&prefix, idx, &preroll) {
                            *self.last_error.lock() = Some(e.to_string());
                            let mut split = self.split.lock();
                            split.phase = SplitPhase::Listening;
                            split.track_index = split.track_index.saturating_sub(1);
                        }
                    }
                } else {
                    split.loud_ms = 0.0;
                }
            }
            SplitPhase::RecordingTrack => {
                drop(split);
                self.append_pcm(block);
                let mut split = self.split.lock();
                if db > self.stop_db {
                    split.quiet_ms = 0.0;
                    if let Some(a) = self.inner.lock().as_mut() {
                        a.loud_end_bytes = a.data_bytes;
                    }
                } else {
                    split.quiet_ms += block_ms;
                    if split.quiet_ms >= self.stop_hold_ms {
                        split.phase = SplitPhase::Listening;
                        split.loud_ms = 0.0;
                        split.quiet_ms = 0.0;
                        split.preroll.clear();
                        split.preroll_bytes = 0;
                        drop(split);
                        if let Some(active) = self.inner.lock().take() {
                            match finalize_active(active, self.tail_bytes(), true) {
                                Ok(fin) => {
                                    if fin.duration_sec >= self.min_track_sec {
                                        if let Some(n) = fin
                                            .path
                                            .file_name()
                                            .map(|s| s.to_string_lossy().into_owned())
                                        {
                                            *self.last_finished.lock() = Some(n);
                                        }
                                    } else {
                                        let _ = fs::remove_file(&fin.path);
                                        let mut split = self.split.lock();
                                        split.track_index =
                                            split.track_index.saturating_sub(1);
                                        tracing::info!(
                                            path = %fin.path.display(),
                                            "discarded short track"
                                        );
                                    }
                                }
                                Err(e) => {
                                    *self.last_error.lock() = Some(e.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn open_track_file(&self, prefix: &str, idx: u32, preroll: &[Vec<u8>]) -> Result<()> {
        let name = sanitize_filename(&format!("{prefix}-t{idx:02}.wav"));
        let path = self.dir.join(&name);
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
        let mut data_bytes = 0u64;
        for chunk in preroll {
            file.write_all(chunk)?;
            data_bytes += chunk.len() as u64;
        }
        *self.inner.lock() = Some(ActiveRecording {
            path: path.clone(),
            file,
            data_bytes,
            sample_rate: self.sample_rate,
            channels: self.channels,
            bits: self.bits,
            loud_end_bytes: data_bytes,
        });
        tracing::info!(path = %path.display(), "track recording started");
        Ok(())
    }

    fn append_pcm(&self, pcm: &[u8]) {
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

    fn tail_bytes(&self) -> u64 {
        let frame = frame_bytes(self.channels, self.bits) as f64;
        (self.tail_ms / 1000.0 * self.sample_rate as f64 * frame) as u64
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
        let canon_dir = self.dir.canonicalize().unwrap_or_else(|_| self.dir.clone());
        let canon = path.canonicalize().unwrap_or(path.clone());
        if !canon.starts_with(&canon_dir) {
            bail!("invalid path");
        }
        Ok(canon)
    }

    pub fn record_dir(&self) -> &Path {
        &self.dir
    }
}

struct FinalizedTrack {
    path: PathBuf,
    #[allow(dead_code)]
    size_bytes: u64,
    duration_sec: f64,
}

fn finalize_active(
    mut active: ActiveRecording,
    tail_bytes: u64,
    trim_to_loud: bool,
) -> Result<FinalizedTrack> {
    let mut data_bytes = active.data_bytes;
    if trim_to_loud && active.loud_end_bytes > 0 && active.loud_end_bytes < data_bytes {
        let keep = active
            .loud_end_bytes
            .saturating_add(tail_bytes)
            .min(data_bytes);
        if keep < data_bytes {
            data_bytes = keep;
            let total = 44 + data_bytes;
            active.file.set_len(total)?;
        }
    }
    write_wav_header(
        &mut active.file,
        active.sample_rate,
        active.channels,
        active.bits,
        data_bytes as u32,
    )?;
    active.file.flush()?;
    let frame = frame_bytes(active.channels, active.bits) as u64;
    let byte_rate = active.sample_rate as u64 * frame;
    let duration_sec = if byte_rate > 0 {
        data_bytes as f64 / byte_rate as f64
    } else {
        0.0
    };
    let size_bytes = 44 + data_bytes;
    tracing::info!(
        path = %active.path.display(),
        bytes = data_bytes,
        duration_sec,
        "recording finalized"
    );
    Ok(FinalizedTrack {
        path: active.path,
        size_bytes,
        duration_sec,
    })
}

fn frame_bytes(channels: u16, bits: u16) -> usize {
    channels as usize * (bits as usize / 8)
}

fn block_dbfs(buf: &[u8], bits: u16) -> f64 {
    let bps = (bits / 8) as usize;
    if bps == 0 || buf.len() < bps {
        return -120.0;
    }
    let mut acc = 0.0f64;
    let mut count = 0usize;
    let mut i = 0usize;
    let fullscale = match bits {
        16 => 32768.0,
        24 => 8_388_608.0,
        32 => 2_147_483_648.0,
        _ => 8_388_608.0,
    };
    while i + bps <= buf.len() {
        let s = match bps {
            2 => i16::from_le_bytes([buf[i], buf[i + 1]]) as f64,
            3 => {
                let b0 = buf[i] as i32;
                let b1 = buf[i + 1] as i32;
                let b2 = buf[i + 2] as i8 as i32;
                ((b2 << 16) | (b1 << 8) | b0) as f64
            }
            4 => i32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as f64,
            _ => 0.0,
        };
        acc += s * s;
        count += 1;
        i += bps;
    }
    if count == 0 {
        return -120.0;
    }
    let rms = (acc / count as f64).sqrt();
    if rms < 1.0 {
        return -120.0;
    }
    20.0 * (rms / fullscale).log10()
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
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}
