use super::AudioReader;
use crate::session::Cancellation;
use std::cmp::Ordering;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_QUEUE_ENTRIES: usize = 10_000;
const MAX_FIELD_CHARS: usize = 256;
const MUSIC_EXTENSIONS: [&str; 4] = ["flac", "m4a", "mp3", "wav"];

#[derive(Clone, Debug, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub duration: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub path: PathBuf,
    pub metadata: TrackMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Unreadable(String),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanReport {
    pub tracks: Vec<Track>,
    pub skipped: Vec<Skipped>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanOptions {
    pub recursive: bool,
    pub max_entries: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { recursive: false, max_entries: MAX_QUEUE_ENTRIES }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanError {
    #[error("scan cancelled")]
    Cancelled,
    #[error("request exceeds {limit} entries")]
    TooManyEntries { limit: usize },
}

pub fn scan(paths: &[PathBuf], options: &ScanOptions, cancel: &Cancellation) -> Result<ScanReport, ScanError> {
    let limit = options.max_entries.min(MAX_QUEUE_ENTRIES);
    let mut found = Found { limit, candidates: Vec::new(), skipped: Vec::new() };
    for path in paths {
        check(cancel)?;
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => found.folder(path, options.recursive, cancel)?,
            Ok(_) => found.candidate(path.clone())?,
            Err(e) => found.skip(path.clone(), SkipReason::Unreadable(e.to_string()))?,
        }
    }
    let mut report = ScanReport { tracks: Vec::with_capacity(found.candidates.len()), skipped: found.skipped };
    for path in found.candidates {
        check(cancel)?;
        match probe(&path) {
            Ok(metadata) => report.tracks.push(Track { path, metadata }),
            Err(reason) => report.skipped.push(Skipped { path, reason }),
        }
    }
    Ok(report)
}

struct Found {
    limit: usize,
    candidates: Vec<PathBuf>,
    skipped: Vec<Skipped>,
}

impl Found {
    fn reserve(&self, extra: usize) -> Result<(), ScanError> {
        if self.candidates.len() + self.skipped.len() + extra > self.limit {
            return Err(ScanError::TooManyEntries { limit: self.limit });
        }
        Ok(())
    }

    fn candidate(&mut self, path: PathBuf) -> Result<(), ScanError> {
        self.reserve(1)?;
        self.candidates.push(path);
        Ok(())
    }

    fn skip(&mut self, path: PathBuf, reason: SkipReason) -> Result<(), ScanError> {
        self.reserve(1)?;
        self.skipped.push(Skipped { path, reason });
        Ok(())
    }

    fn folder(&mut self, root: &Path, recursive: bool, cancel: &Cancellation) -> Result<(), ScanError> {
        let mut files = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(rel) = pending.pop() {
            check(cancel)?;
            let dir = root.join(&rel);
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    self.skip(dir, SkipReason::Unreadable(e.to_string()))?;
                    continue;
                }
            };
            for entry in entries {
                check(cancel)?;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        self.skip(dir.clone(), SkipReason::Unreadable(e.to_string()))?;
                        continue;
                    }
                };
                let rel = rel.join(entry.file_name());
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(e) => {
                        self.skip(root.join(rel), SkipReason::Unreadable(e.to_string()))?;
                        continue;
                    }
                };
                if kind.is_dir() {
                    if recursive {
                        pending.push(rel);
                    }
                    continue;
                }
                if !is_music(&rel) {
                    continue;
                }
                if kind.is_symlink() {
                    match fs::metadata(entry.path()) {
                        Ok(meta) if meta.is_file() => {}
                        Ok(_) => continue,
                        Err(e) => {
                            self.skip(root.join(rel), SkipReason::Unreadable(e.to_string()))?;
                            continue;
                        }
                    }
                } else if !kind.is_file() {
                    continue;
                }
                self.reserve(files.len() + 1)?;
                files.push(rel);
            }
        }
        files.sort_by(|a, b| natural_path_cmp(a, b));
        self.candidates.extend(files.into_iter().map(|rel| root.join(rel)));
        Ok(())
    }
}

fn check(cancel: &Cancellation) -> Result<(), ScanError> {
    if cancel.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    Ok(())
}

fn is_music(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MUSIC_EXTENSIONS.iter().any(|m| e.eq_ignore_ascii_case(m)))
}

fn probe(path: &Path) -> Result<TrackMetadata, SkipReason> {
    let file = File::open(path).map_err(|e| SkipReason::Unreadable(e.to_string()))?;
    let ext = path.extension().and_then(|e| e.to_str());
    let unsupported = |e: anyhow::Error| SkipReason::Unsupported(format!("{e:#}"));
    let mut reader = AudioReader::from_source(Box::new(file), ext).map_err(unsupported)?;
    if reader.next_block().map_err(unsupported)?.is_none() {
        return Err(SkipReason::Unsupported("no audio frames".into()));
    }
    let info = reader.info();
    let tag = |key: &str| {
        info.tags
            .iter()
            .find(|(k, v)| k == key && !v.trim().is_empty())
            .map(|(_, v)| bounded(v.trim()))
    };
    let fallback = path.file_stem().or_else(|| path.file_name()).unwrap_or(path.as_os_str());
    Ok(TrackMetadata {
        title: tag("title").unwrap_or_else(|| bounded(&fallback.to_string_lossy())),
        artist: tag("artist"),
        album: tag("album"),
        track: tag("track").and_then(|v| v.split('/').next()?.trim().parse().ok()),
        duration: info.duration_secs().and_then(|s| Duration::try_from_secs_f64(s).ok()),
    })
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_FIELD_CHARS).collect()
}

fn natural_path_cmp(a: &Path, b: &Path) -> Ordering {
    let mut x = a.components();
    let mut y = b.components();
    loop {
        match (x.next(), y.next()) {
            (None, None) => return a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes()),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) => {
                let ord = natural_cmp(&c.as_os_str().to_string_lossy(), &d.as_os_str().to_string_lossy());
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        let ord = match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let m = digits(&mut x);
                let n = digits(&mut y);
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                m.len().cmp(&n.len()).then_with(|| m.cmp(n))
            }
            (Some(c), Some(d)) => {
                x.next();
                y.next();
                c.to_lowercase().cmp(d.to_lowercase())
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_digit()) {
        run.push(c);
    }
    run
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(u64);

impl EntryId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueEntry {
    pub id: EntryId,
    pub path: PathBuf,
    pub metadata: TrackMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("queue would exceed {limit} entries")]
    Full { limit: usize },
    #[error("unknown queue entry {0:?}")]
    UnknownEntry(EntryId),
    #[error("index {index} out of range for queue of {len}")]
    IndexOutOfRange { index: usize, len: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cursor {
    Start,
    At(EntryId),
    Before(EntryId),
    End,
}

#[derive(Clone, Debug)]
pub struct Queue {
    entries: Vec<QueueEntry>,
    cursor: Cursor,
    next_id: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self { entries: Vec::new(), cursor: Cursor::Start, next_id: 1 }
    }
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[QueueEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: EntryId) -> Option<&QueueEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn position(&self, id: EntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        match self.cursor {
            Cursor::At(id) => self.get(id),
            _ => None,
        }
    }

    pub fn append(&mut self, tracks: Vec<Track>) -> Result<Vec<EntryId>, QueueError> {
        self.insert(self.entries.len(), tracks)
    }

    pub fn insert(&mut self, index: usize, tracks: Vec<Track>) -> Result<Vec<EntryId>, QueueError> {
        let len = self.entries.len();
        if index > len {
            return Err(QueueError::IndexOutOfRange { index, len });
        }
        if len + tracks.len() > MAX_QUEUE_ENTRIES {
            return Err(QueueError::Full { limit: MAX_QUEUE_ENTRIES });
        }
        let start = self.next_id;
        self.next_id += tracks.len() as u64;
        let added: Vec<QueueEntry> = tracks
            .into_iter()
            .zip(start..)
            .map(|(t, id)| QueueEntry { id: EntryId(id), path: t.path, metadata: t.metadata })
            .collect();
        let ids: Vec<EntryId> = added.iter().map(|e| e.id).collect();
        if let (Cursor::End, Some(&first)) = (self.cursor, ids.first()) {
            if index == len {
                self.cursor = Cursor::Before(first);
            }
        }
        self.entries.splice(index..index, added);
        Ok(ids)
    }

    pub fn remove(&mut self, id: EntryId) -> Option<QueueEntry> {
        let pos = self.position(id)?;
        let entry = self.entries.remove(pos);
        if matches!(self.cursor, Cursor::At(c) | Cursor::Before(c) if c == id) {
            self.cursor = self.entries.get(pos).map_or(Cursor::End, |e| Cursor::Before(e.id));
        }
        Some(entry)
    }

    pub fn move_to(&mut self, id: EntryId, index: usize) -> Result<(), QueueError> {
        let pos = self.position(id).ok_or(QueueError::UnknownEntry(id))?;
        let len = self.entries.len();
        if index >= len {
            return Err(QueueError::IndexOutOfRange { index, len });
        }
        let entry = self.entries.remove(pos);
        self.entries.insert(index, entry);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = Cursor::Start;
    }

    pub fn select(&mut self, id: EntryId) -> Result<&QueueEntry, QueueError> {
        let pos = self.position(id).ok_or(QueueError::UnknownEntry(id))?;
        self.cursor = Cursor::At(id);
        Ok(&self.entries[pos])
    }

    pub fn peek_next(&self) -> Option<EntryId> {
        match self.cursor {
            Cursor::Start => self.entries.first().map(|e| e.id),
            Cursor::At(id) => self.entries.get(self.position(id)? + 1).map(|e| e.id),
            Cursor::Before(id) => Some(id),
            Cursor::End => None,
        }
    }

    pub fn peek_previous(&self) -> Option<EntryId> {
        match self.cursor {
            Cursor::Start => None,
            Cursor::At(id) | Cursor::Before(id) => self.entries.get(self.position(id)?.checked_sub(1)?).map(|e| e.id),
            Cursor::End => self.entries.last().map(|e| e.id),
        }
    }

    pub fn advance(&mut self) -> Option<&QueueEntry> {
        let id = self.peek_next()?;
        self.select(id).ok()
    }

    pub fn retreat(&mut self) -> Option<&QueueEntry> {
        let id = self.peek_previous()?;
        self.select(id).ok()
    }
}
