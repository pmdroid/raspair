use crate::play::{CANCELLED, FAILED, USAGE};
use airplay_core::audio::{scan, subfolders, AudioReader, EntryId, Queue, QueueEntry, ScanOptions, ScanReport, SkipReason, Skipped, Track};
use airplay_core::pairing::{CredentialStore, FileStore};
use airplay_core::discovery::Device;
use airplay_core::pairing::MemoryStore;
use airplay_core::player::{connect_device, discover_device, MIN_VOLUME_DB};
use airplay_core::protocol::HOMEPOD_PIN;
use airplay_core::{
    Cancellation, Command, CommandError, CommandId, CommandState, Player, PlayerEvent, PlayerState, PlayerStatus, ProfileSelection, QualityReport, SessionOptions, SourceFormat, Volume,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod picker;
use picker::{Job, Picker, Worker};
#[cfg(test)]
use picker::Outcome;

pub const FRAME: Duration = Duration::from_millis(50);
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const SEEK_STEP: Duration = Duration::from_secs(10);
const VOLUME_STEP: f64 = 2.0;
const NARROW: u16 = 72;
const KEYS: [(&str, &str); 20] = [
    ("Enter", "open folder / play"),
    ("↑↓ j k", "select"),
    ("a", "add file or folder"),
    ("/", "search this folder"),
    ("Backspace", "parent folder"),
    ("x Del", "remove from queue"),
    ("K J", "move up / down"),
    ("c", "clear all but playing"),
    ("d", "receivers"),
    ("Space", "play / pause"),
    ("← / →", "seek 10 s"),
    ("p / n", "previous / next"),
    ("s", "stop"),
    ("- / +", "volume"),
    ("m", "mute"),
    ("r", "retry after error"),
    ("Tab", "move focus"),
    ("?", "toggle help"),
    ("q / Esc", "quit"),
    ("Ctrl-C", "quit"),
];
const CONTROLS: &str = "Space play/pause  ←/→ seek  p/n prev/next  s stop  -/+ vol  m mute";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Plain,
    Basic,
    Indexed,
    Rgb,
}

impl Palette {
    pub fn detect(get: impl Fn(&str) -> Option<String>) -> Self {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::Plain;
        }
        match (get("COLORTERM").unwrap_or_default().as_str(), get("TERM").unwrap_or_default()) {
            ("truecolor" | "24bit", _) => Self::Rgb,
            (_, term) if term.contains("256") => Self::Indexed,
            _ => Self::Basic,
        }
    }

    fn color(self, rgb: u32, indexed: u8, basic: Color) -> Option<Color> {
        match self {
            Self::Plain => None,
            Self::Basic => Some(basic),
            Self::Indexed => Some(Color::Indexed(indexed)),
            Self::Rgb => Some(Color::from_u32(rgb)),
        }
    }

    fn fg(self, rgb: u32, indexed: u8, basic: Color) -> Style {
        self.color(rgb, indexed, basic).map_or_else(Style::default, |c| Style::default().fg(c))
    }

    fn base(self) -> Style {
        match self {
            Self::Plain | Self::Basic => Style::default(),
            _ => self.fg(0x3e4238, 237, Color::Reset).bg(self.color(0xfffcf5, 231, Color::Reset).unwrap()),
        }
    }
    fn accent(self) -> Style {
        self.fg(0x646c42, 58, Color::Green)
    }
    fn muted(self) -> Style {
        self.fg(0x6c6e5f, 242, Color::DarkGray)
    }
    fn warm(self) -> Style {
        self.fg(0xa25331, 130, Color::Yellow)
    }
    fn error(self) -> Style {
        self.fg(0x9a4434, 131, Color::Red)
    }
    fn focus(self) -> Style {
        self.fg(0xba873b, 136, Color::Yellow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Browser,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Quit(i32),
    Send(Command),
    Retry(EntryId),
    Play(EntryId),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pending {
    pub id: CommandId,
    pub command: Command,
    pub entry: Option<EntryId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Up(PathBuf),
    Folder(PathBuf),
    Track(Track),
    Skipped(Skipped),
}

impl Row {
    fn matches(&self, needle: &str) -> bool {
        let text = match self {
            Row::Up(_) => return true,
            Row::Folder(path) | Row::Skipped(Skipped { path, .. }) => name(path),
            Row::Track(t) => format!("{} {} {}", t.metadata.title, t.metadata.artist.as_deref().unwrap_or(""), name(&t.path)),
        };
        text.to_lowercase().contains(needle)
    }
}

enum Found {
    Listing(Vec<PathBuf>, PathBuf, ScanReport),
    Tracks(ScanReport),
}

pub struct Scan {
    label: String,
    cancel: Cancellation,
    rx: mpsc::Receiver<Result<Found, String>>,
}

pub struct View {
    pub dir: Option<PathBuf>,
    pub rows: Vec<Row>,
    pub browse_at: usize,
    pub search: Option<String>,
    pub searching: bool,
    pub scan: Option<Scan>,
    pub recursive: bool,
    pub device: Option<String>,
    pub queue: Queue,
    pub queue_at: Option<EntryId>,
    pub stale: bool,
    pub note: Option<String>,
    pub state: Option<PlayerState>,
    pub current: Option<EntryId>,
    pub position: Duration,
    pub epoch: u64,
    pub selection: Option<ProfileSelection>,
    pub source: Option<SourceFormat>,
    probed: Option<EntryId>,
    pub volume: Option<Volume>,
    pub levels: [u8; 2],
    pub pending: Vec<Pending>,
    pub notice: Option<String>,
    pub focus: Pane,
    pub help: bool,
    pub palette: Palette,
    pub reduced_motion: bool,
    pub tick: usize,
    pub frames: u64,
    pub picker: Picker,
    pub jobs: Vec<Job>,
    pub switched: bool,
    pub play: bool,
}

type Start = Box<dyn FnMut(Queue, Device, Vec<Device>) -> Player>;

pub struct Deck {
    pub worker: Option<Worker>,
    pub start: Option<Start>,
}

fn meter(peak: f32) -> u8 {
    ((20.0 * peak.max(1e-6).log10() + 48.0) / 6.0).clamp(0.0, 8.0) as u8
}

fn probe(entry: Option<&QueueEntry>) -> Option<SourceFormat> {
    entry.and_then(|e| AudioReader::open(&e.path).ok()).map(|r| SourceFormat::from(r.info()))
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn origin(path: &Path) -> String {
    let parent = path.parent().and_then(|p| p.file_name()).map(|n| format!("{}/", n.to_string_lossy())).unwrap_or_default();
    format!("{parent}{}", name(path))
}

fn reason(skipped: &Skipped) -> String {
    match &skipped.reason {
        SkipReason::Unreadable(m) => format!("unreadable: {m}"),
        SkipReason::Unsupported(m) => format!("unsupported: {m}"),
    }
}

fn dupes<'a>(titles: impl Iterator<Item = &'a str>) -> HashMap<&'a str, usize> {
    let mut seen = HashMap::new();
    titles.for_each(|t| *seen.entry(t).or_insert(0) += 1);
    seen
}

fn window<T>(items: Vec<T>, at: usize, area: Rect) -> Vec<T> {
    let height = area.height.saturating_sub(2).max(1) as usize;
    items.into_iter().skip((at + 1).saturating_sub(height)).take(height).collect()
}

impl View {
    pub fn new(dir: Option<PathBuf>, device: Option<String>, queue: Queue, palette: Palette, reduced_motion: bool) -> Self {
        let current = queue.entries().first().map(|e| e.id);
        Self {
            dir,
            rows: Vec::new(),
            browse_at: 0,
            search: None,
            searching: false,
            scan: None,
            recursive: false,
            device,
            queue,
            queue_at: current,
            stale: false,
            note: None,
            state: None,
            current,
            position: Duration::ZERO,
            epoch: 0,
            selection: None,
            source: None,
            probed: None,
            volume: None,
            levels: [0; 2],
            pending: Vec::new(),
            notice: None,
            focus: Pane::Queue,
            help: false,
            palette,
            reduced_motion,
            tick: 0,
            frames: 0,
            picker: Picker::new(Arc::new(MemoryStore::default())),
            jobs: Vec::new(),
            switched: false,
            play: false,
        }
    }

    fn select(&mut self, id: EntryId) -> bool {
        if self.current == Some(id) && self.probed == Some(id) {
            return false;
        }
        if self.current != Some(id) {
            self.pending.retain(|p| !matches!(p.command, Command::Seek(_)) || p.entry == Some(id));
        }
        (self.current, self.probed) = (Some(id), Some(id));
        let _ = self.queue.select(id);
        self.source = probe(self.queue.get(id));
        true
    }

    pub fn update(&mut self, status: PlayerStatus, events: Vec<PlayerEvent>) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                PlayerEvent::TrackAudible(id) => changed |= self.select(id),
                PlayerEvent::Command { id, state } => {
                    let Some(i) = self.pending.iter().position(|p| p.id == id) else { continue };
                    if let (CommandState::Failed(error), true) = (state, self.pending.remove(i).entry == self.current) {
                        self.notice = Some(error.to_string());
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        if status.position.epoch < self.epoch {
            return changed;
        }
        self.epoch = status.position.epoch;
        if let Some(id) = status.position.entry {
            changed |= self.select(id);
        }
        let position = Duration::from_millis(status.position.audible_frames * 1000 / 48_000);
        let selection = status.profile.or(self.selection);
        let levels = status.levels.map(meter);
        let next = (Some(status.state), position.as_secs(), selection, Some(status.volume), levels);
        if next != (self.state.clone(), self.position.as_secs(), self.selection, self.volume, self.levels) {
            (self.state, _, self.selection, self.volume, self.levels) = next;
            (self.position, changed) = (position, true);
        }
        changed
    }

    pub fn sent(&mut self, command: Command, result: Result<CommandId, CommandError>) {
        match result {
            Ok(id) => {
                self.pending.push(Pending { id, command, entry: self.current });
                self.notice = None;
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    pub fn restarted(&mut self) {
        (self.state, self.epoch, self.selection, self.notice, self.stale) = (None, 0, None, None, false);
        self.pending.clear();
    }

    fn latest<T>(&self, pick: impl Fn(Command) -> Option<T>) -> Option<T> {
        self.pending.iter().rev().filter(|p| p.entry == self.current).find_map(|p| pick(p.command))
    }

    fn transport(&self, code: KeyCode) -> Option<Command> {
        let duration = self.current.and_then(|id| self.queue.get(id)).and_then(|e| e.metadata.duration);
        let at = self.latest(|c| if let Command::Seek(d) = c { Some(d) } else { None }).unwrap_or(self.position);
        let volume = self.volume.map(|v| Volume { db: self.latest(|c| if let Command::Volume(db) = c { Some(db) } else { None }).unwrap_or(v.db), muted: self.latest(|c| if let Command::Mute(m) = c { Some(m) } else { None }).unwrap_or(v.muted) });
        let step = |delta: f64| volume.map(|v| (v.db + delta).clamp(MIN_VOLUME_DB, 0.0)).filter(|db| volume.is_some_and(|v| v.db != *db)).map(Command::Volume);
        match code {
            KeyCode::Char(' ') => match self.state {
                Some(PlayerState::Connecting | PlayerState::Buffering | PlayerState::Playing) => Some(Command::Pause),
                Some(PlayerState::Pausing { .. } | PlayerState::Held { .. }) => Some(Command::Resume),
                _ => None,
            },
            KeyCode::Right => Some(Command::Seek(duration.map_or(at + SEEK_STEP, |d| (at + SEEK_STEP).min(d)))),
            KeyCode::Left => Some(Command::Seek(at.saturating_sub(SEEK_STEP))),
            KeyCode::Char('n') => Some(Command::Next),
            KeyCode::Char('p') => Some(Command::Previous),
            KeyCode::Char('s') => Some(Command::Stop),
            KeyCode::Char('+' | '=') => step(VOLUME_STEP),
            KeyCode::Char('-' | '_') => step(-VOLUME_STEP),
            KeyCode::Char('m') => volume.map(|v| Command::Mute(!v.muted)),
            _ => None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.searching && !ctrl && !self.picker.open {
            let search = self.search.get_or_insert_default();
            match key.code {
                KeyCode::Char(c) => search.push(c),
                KeyCode::Backspace => _ = search.pop(),
                KeyCode::Esc => (self.search, self.searching) = (None, false),
                KeyCode::Enter => self.searching = false,
                _ => {}
            }
            self.browse_at = 0;
            return None;
        }
        let (browser, queue) = (self.focus == Pane::Browser, self.focus == Pane::Queue);
        match key.code {
            KeyCode::Char('c') if ctrl => return Some(Action::Quit(CANCELLED)),
            _ if self.picker.open => {
                let before = self.picker.chosen.clone();
                self.jobs.extend(self.picker.key(key));
                if self.picker.chosen != before {
                    (self.device, self.switched) = (self.picker.chosen_label(), true);
                }
            }
            KeyCode::Char('d') => {
                self.picker.open = true;
                self.jobs.push(Job::Refresh);
            }
            KeyCode::Char(' ') if !matches!(self.state, Some(PlayerState::Connecting | PlayerState::Buffering | PlayerState::Playing | PlayerState::Pausing { .. } | PlayerState::Held { .. })) => self.play = true,
            KeyCode::Esc if self.help => self.help = false,
            KeyCode::Esc if self.cancel_scan() => {}
            KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Quit(0)),
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Tab | KeyCode::BackTab => self.focus = if queue { Pane::Browser } else { Pane::Queue },
            KeyCode::Up | KeyCode::Down if queue && key.modifiers.contains(KeyModifiers::SHIFT) => self.shift(key.code == KeyCode::Down),
            KeyCode::Char(c @ ('K' | 'J')) if queue => self.shift(c == 'J'),
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::Enter => return self.enter(),
            KeyCode::Char('a') if browser => self.add(),
            KeyCode::Char('/') if browser => (self.searching, self.search, self.browse_at) = (true, Some(String::new()), 0),
            KeyCode::Backspace if browser => {
                if let Some(Row::Up(parent)) = self.rows.first().cloned() {
                    self.open(parent);
                }
            }
            KeyCode::Char('x') | KeyCode::Delete if queue => self.remove(),
            KeyCode::Char('c') if queue => self.clear(),
            KeyCode::Char('r') => {
                if let Some(PlayerState::Paused { entry, .. }) = self.state {
                    return Some(Action::Retry(entry));
                }
            }
            code => return self.transport(code).map(Action::Send),
        }
        None
    }

    fn queue_from_current(&self) -> Queue {
        self.current.or(self.queue.entries().first().map(|e| e.id)).map_or_else(|| self.queue.clone(), |id| resume_at(&self.queue, id))
    }

    pub fn visible(&self) -> Vec<&Row> {
        let needle = self.search.as_deref().unwrap_or("").to_lowercase();
        self.rows.iter().filter(|r| r.matches(&needle)).collect()
    }

    pub fn open(&mut self, dir: PathBuf) {
        self.start(dir, false);
    }

    fn start(&mut self, path: PathBuf, add: bool) {
        self.cancel_scan();
        let (cancel, (tx, rx)) = (Cancellation::default(), mpsc::channel());
        let options = ScanOptions { recursive: add && self.recursive, ..ScanOptions::default() };
        let label = format!("{} {}", if add { "adding" } else { "opening" }, name(&path));
        let c = cancel.clone();
        std::thread::spawn(move || {
            let report = scan(std::slice::from_ref(&path), &options, &c);
            let found = report.map(|r| if add { Found::Tracks(r) } else { Found::Listing(subfolders(&path), path, r) });
            let _ = tx.send(found.map_err(|e| e.to_string()));
        });
        self.scan = Some(Scan { label, cancel, rx });
    }

    fn cancel_scan(&mut self) -> bool {
        let Some(scan) = self.scan.take() else { return false };
        scan.cancel.cancel();
        self.note = Some(format!("cancelled {}", scan.label));
        true
    }

    pub fn poll(&mut self) -> bool {
        let Some(job) = &self.scan else { return false };
        let found = match job.rx.try_recv() {
            Ok(found) => found,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("scan stopped".into()),
        };
        self.scan = None;
        match found {
            Ok(Found::Listing(folders, dir, report)) => {
                let up = dir.parent().map(|p| Row::Up(p.to_path_buf()));
                let rows = up.into_iter().chain(folders.into_iter().map(Row::Folder)).chain(report.tracks.into_iter().map(Row::Track));
                self.rows = rows.chain(report.skipped.into_iter().map(Row::Skipped)).collect();
                (self.dir, self.browse_at, self.search, self.note) = (Some(dir), 0, None, None);
            }
            Ok(Found::Tracks(report)) => {
                let (added, skipped) = (report.tracks.len(), report.skipped.len());
                self.note = Some(match self.queue.append(report.tracks) {
                    Ok(_) if skipped > 0 => format!("added {added}, skipped {skipped} unplayable"),
                    Ok(_) => format!("added {added}"),
                    Err(e) => e.to_string(),
                });
                self.edited();
            }
            Err(e) => self.note = Some(e),
        }
        true
    }

    fn selected(&self) -> Option<Row> {
        self.visible().get(self.browse_at).map(|r| (*r).clone())
    }

    fn step(&mut self, down: bool) {
        let next = |i: usize, len: usize| if down { (i + 1).min(len.saturating_sub(1)) } else { i.saturating_sub(1) };
        match self.focus {
            Pane::Browser => self.browse_at = next(self.browse_at, self.visible().len()),
            Pane::Queue => {
                let at = self.queue_at.and_then(|id| self.queue.position(id)).unwrap_or(0);
                self.queue_at = self.queue.entries().get(next(at, self.queue.len())).map(|e| e.id);
            }
        }
    }

    fn enter(&mut self) -> Option<Action> {
        match (self.focus, self.selected()) {
            (Pane::Queue, _) => return self.queue_at.map(Action::Play),
            (_, Some(Row::Up(path) | Row::Folder(path))) => self.open(path),
            (_, Some(Row::Track(track))) => match self.queue.append(vec![track]) {
                Ok(ids) => {
                    self.edited();
                    return Some(Action::Play(ids[0]));
                }
                Err(e) => self.note = Some(e.to_string()),
            },
            (_, Some(Row::Skipped(s))) => self.note = Some(reason(&s)),
            (_, None) => {}
        }
        None
    }

    fn add(&mut self) {
        match self.selected() {
            Some(Row::Folder(path)) => self.start(path, true),
            Some(Row::Track(track)) => {
                let title = track.metadata.title.clone();
                self.note = Some(self.queue.append(vec![track]).map_or_else(|e| e.to_string(), |_| format!("added {title}")));
                self.edited();
            }
            Some(Row::Skipped(s)) => self.note = Some(reason(&s)),
            _ => {}
        }
    }

    fn remove(&mut self) {
        let Some(id) = self.queue_at else { return };
        if Some(id) == self.current {
            self.note = Some("the playing track stays queued".into());
            return;
        }
        let at = self.queue.position(id).unwrap_or(0);
        self.queue.remove(id);
        self.queue_at = self.queue.entries().get(at.min(self.queue.len().saturating_sub(1))).map(|e| e.id);
        self.edited();
    }

    fn shift(&mut self, down: bool) {
        let Some((id, at)) = self.queue_at.and_then(|id| Some((id, self.queue.position(id)?))) else { return };
        if self.queue.move_to(id, if down { at + 1 } else { at.wrapping_sub(1) }).is_ok() {
            self.edited();
        }
    }

    fn clear(&mut self) {
        let keep = self.current.filter(|id| self.queue.get(*id).is_some());
        let others: Vec<EntryId> = self.queue.entries().iter().map(|e| e.id).filter(|id| Some(*id) != keep).collect();
        match keep {
            None => self.queue.clear(),
            Some(_) => others.into_iter().rev().for_each(|id| _ = self.queue.remove(id)),
        }
        self.queue_at = keep;
        self.edited();
    }

    fn edited(&mut self) {
        if self.queue_at.and_then(|id| self.queue.get(id)).is_none() {
            self.queue_at = self.queue.entries().first().map(|e| e.id);
        }
        self.stale = matches!(self.state, Some(PlayerState::Connecting | PlayerState::Buffering | PlayerState::Playing | PlayerState::Pausing { .. } | PlayerState::Held { .. }));
    }

    fn hints(&self, width: u16) -> &'static str {
        let (full, short) = match (self.searching, self.focus) {
            (true, _) => ("  type to filter  Enter keep  Esc clear ", " ↵ keep  Esc clear "),
            (_, Pane::Browser) => ("  Enter open/play  a add  / search  ⌫ up  d receivers  ? help ", " ↵ open a add / find ⌫ up d rx ? help "),
            (_, Pane::Queue) => ("  Enter play  x remove  K/J move  c clear  d receivers  ? help ", " ↵ play x del K/J move c clr ? help "),
        };
        if Line::raw(full).width() + 20 <= width as usize { full } else { short }
    }


    fn spinner(&self) -> &'static str {
        if self.reduced_motion { "…" } else { SPINNER[self.tick / 4 % 4] }
    }

    fn animating(&self) -> bool {
        !self.reduced_motion && (self.scan.is_some() || matches!(self.state, Some(PlayerState::Connecting | PlayerState::Buffering | PlayerState::Pausing { .. })))
    }

    fn badge(&self) -> (String, Style) {
        let p = self.palette;
        let spin = self.spinner();
        let (text, style) = match &self.state {
            None => ("○ IDLE".into(), p.muted()),
            Some(PlayerState::Connecting) => (format!("{spin} CONNECTING"), p.warm()),
            Some(PlayerState::Buffering) => (format!("{spin} BUFFERING"), p.warm()),
            Some(PlayerState::Playing) => ("▶ PLAYING".into(), p.accent()),
            Some(PlayerState::Pausing { .. }) => (format!("{spin} PAUSING"), p.warm()),
            Some(PlayerState::Held { .. }) => ("‖ PAUSED".into(), p.warm()),
            Some(PlayerState::Finished) => ("✓ ENDED".into(), p.muted()),
            Some(PlayerState::Stopped) => ("■ STOPPED".into(), p.muted()),
            Some(PlayerState::Paused { .. }) => ("✕ FAILED".into(), p.error()),
        };
        (text, style.add_modifier(Modifier::BOLD))
    }

    fn status_line(&self) -> Line<'static> {
        let p = self.palette;
        if let Some(PlayerState::Paused { error, .. }) = &self.state {
            return Line::from(vec![Span::styled(format!("{}  ", self.picker.describe(error)), p.error()), Span::styled("r retry", p.focus().add_modifier(Modifier::BOLD))]);
        }
        if let Some(notice) = &self.notice {
            return Line::styled(format!("✕ {notice}"), p.error());
        }
        if !self.pending.is_empty() {
            let what: Vec<String> = self.pending.iter().map(|p| describe(p.command)).collect();
            return Line::styled(format!("⋯ {}", what.join(" · ")), p.warm());
        }
        if self.reduced_motion || self.state != Some(PlayerState::Playing) {
            return Line::raw("");
        }
        let bar = |n: u8| format!("{}{}", "█".repeat(n as usize), "░".repeat(8 - n as usize));
        Line::styled(format!("L {}  R {}", bar(self.levels[0]), bar(self.levels[1])), p.accent())
    }

    fn pane(&self, pane: Pane, title: &str) -> Block<'static> {
        let p = self.palette;
        let (marker, border, kind) = match self.focus == pane {
            true => ("▸ ", p.focus().add_modifier(Modifier::BOLD), BorderType::Thick),
            false => ("", p.muted(), BorderType::Plain),
        };
        Block::bordered().border_type(kind).border_style(border).title(Span::styled(format!(" {marker}{title} "), border))
    }

    fn row(&self, pane: Pane, selected: bool, spans: Vec<Span<'static>>) -> Line<'static> {
        let mut spans = spans;
        spans.insert(0, Span::raw(if selected { "›" } else { " " }));
        let line = Line::from(spans);
        match selected && self.focus == pane {
            true => line.style(Style::default().add_modifier(Modifier::REVERSED)),
            false => line,
        }
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn describe(command: Command) -> String {
    match command {
        Command::Pause => "pausing".into(),
        Command::Resume => "resuming".into(),
        Command::Seek(to) => format!("seeking to {}", clock(to)),
        Command::Next => "next track".into(),
        Command::Previous => "previous track".into(),
        Command::Stop => "stopping".into(),
        Command::Volume(db) => format!("volume {db:.1} dB"),
        Command::Mute(true) => "muting".into(),
        Command::Mute(false) => "unmuting".into(),
    }
}

pub fn quality(selection: &ProfileSelection, source: Option<&SourceFormat>) -> String {
    let e = selection.effective;
    let khz = |rate: u32| rate as f64 / 1000.0;
    let codec = |name: &str| if name.starts_with("pcm") { "PCM".to_string() } else { name.to_uppercase() };
    let text = format!("{} {}-bit/{} kHz · {}", format!("{:?}", e.codec).to_uppercase(), e.valid_bits, khz(e.sample_rate), format!("{:?}", e.transport).to_lowercase());
    let report = source.map(|s| QualityReport::new(s.clone(), e));
    let origin = match &report {
        None => "source unknown".to_string(),
        Some(r) if !r.source.lossless => format!("lossy {} source", codec(&r.source.codec)),
        Some(r) if r.lossless_end_to_end && e.is_buffered_lossless_24_48() && r.source.bits_per_sample == Some(24) => "native lossless".into(),
        Some(r) => {
            let bits = r.source.bits_per_sample.map(|b| format!(" {b}-bit/")).unwrap_or_else(|| " ".into());
            let how = if r.lossless_end_to_end { "lossless" } else { "converted" };
            format!("{how} from {}{bits}{} kHz", codec(&r.source.codec), khz(r.source.sample_rate))
        }
    };
    let fallback = selection.fallback.map(|_| format!(" · fallback from {}", format!("{:?}", selection.requested.codec).to_uppercase())).unwrap_or_default();
    format!("{text} · {origin}{fallback}")
}

fn browser(frame: &mut Frame, view: &View, area: Rect) {
    let p = view.palette;
    let visible = view.visible();
    let names = dupes(visible.iter().filter_map(|r| if let Row::Track(t) = r { Some(t.metadata.title.as_str()) } else { None }));
    let mut lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let spans = match row {
                Row::Up(_) => vec![Span::styled("↰ ..", p.muted())],
                Row::Folder(path) => vec![Span::raw(format!("▹ {}/", name(path)))],
                Row::Track(t) => {
                    let mut extra = t.metadata.artist.as_ref().map(|a| format!(" · {a}")).unwrap_or_default();
                    if names.get(t.metadata.title.as_str()) > Some(&1) {
                        extra.push_str(&format!(" · {}", name(&t.path)));
                    }
                    vec![Span::raw(format!("  {}", t.metadata.title)), Span::styled(extra, p.muted())]
                }
                Row::Skipped(s) => vec![Span::styled(format!("✕ {}", name(&s.path)), p.error()), Span::styled(format!(" {}", reason(s)), p.muted())],
            };
            view.row(Pane::Browser, i == view.browse_at, spans)
        })
        .collect();
    if view.scan.is_none() && visible.iter().all(|r| matches!(r, Row::Up(_))) {
        let empty = match (&view.search, &view.dir) {
            (Some(q), _) if !q.is_empty() => format!(" no matches for “{q}”"),
            (_, Some(_)) => " empty folder · no music here".into(),
            _ => " no folder open".into(),
        };
        lines.push(Line::styled(empty, p.muted()));
    }
    let dir = view.dir.as_deref().map(name).unwrap_or_default();
    let search = match (&view.search, view.searching) {
        (Some(q), true) => format!("  /{q}▏"),
        (Some(q), false) if !q.is_empty() => format!("  /{q}"),
        _ => String::new(),
    };
    let mut block = view.pane(Pane::Browser, &format!("Browse {dir}{search}"));
    if let Some(scan) = &view.scan {
        block = block.title_bottom(Span::styled(format!(" {} {}… Esc cancels ", view.spinner(), scan.label), p.warm()));
    } else if let Some(note) = &view.note {
        block = block.title_bottom(Span::styled(format!(" {note} "), p.muted()));
    } else if !view.picker.status.is_empty() && !view.picker.open {
        block = block.title_bottom(Span::styled(format!(" {} ", view.picker.status), p.warm()));
    }
    frame.render_widget(Paragraph::new(window(lines, view.browse_at, area)).block(block), area);
}

fn queue(frame: &mut Frame, view: &View, area: Rect) {
    let p = view.palette;
    let entries = view.queue.entries();
    let names = dupes(entries.iter().map(|e| e.metadata.title.as_str()));
    let rows: Vec<Line> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let playing = view.current == Some(e.id);
            let length = e.metadata.duration.map(clock).unwrap_or_else(|| "--:--".into());
            let mut extra = e.metadata.artist.as_ref().map(|a| format!(" · {a}")).unwrap_or_default();
            if names.get(e.metadata.title.as_str()) > Some(&1) {
                extra.push_str(&format!(" · {}", origin(&e.path)));
            }
            let style = if playing { p.accent().add_modifier(Modifier::BOLD) } else { Style::default() };
            let text = format!("{}{:>3}. {length:>6}  {}", if playing { "▶" } else { " " }, i + 1, e.metadata.title);
            view.row(Pane::Queue, view.queue_at == Some(e.id), vec![Span::styled(text, style), Span::styled(extra, p.muted())])
        })
        .collect();
    let at = view.queue_at.and_then(|id| view.queue.position(id)).unwrap_or(0);
    let stale = if view.stale { " · edits apply on next play" } else { "" };
    let block = view.pane(Pane::Queue, &format!("Queue ({}){stale}", entries.len()));
    frame.render_widget(Paragraph::new(window(rows, at, area)).block(block), area);
}

pub fn render(frame: &mut Frame, view: &View) {
    let p = view.palette;
    let area = frame.area();
    frame.render_widget(Block::new().style(p.base()), area);
    let [top, now, footer] = Layout::vertical([Constraint::Min(5), Constraint::Length(9), Constraint::Length(1)]).areas(area);
    let [left, right] = match area.width < NARROW {
        true => Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top),
        false => Layout::horizontal([Constraint::Percentage(40), Constraint::Min(20)]).areas(top),
    };
    browser(frame, view, left);
    queue(frame, view, right);

    let entry = view.current.and_then(|id| view.queue.get(id));
    let (badge, badge_style) = view.badge();
    let title = entry.map_or("Nothing queued".to_string(), |e| e.metadata.title.clone());
    let detail = entry.map(|e| [e.metadata.artist.clone(), e.metadata.album.clone()].into_iter().flatten().collect::<Vec<_>>().join(" — ")).unwrap_or_default();
    let width = now.width.saturating_sub(4) as usize;
    let duration = entry.and_then(|e| e.metadata.duration);
    let label = format!(" est. {} / {}", clock(view.position), duration.map(clock).unwrap_or_else(|| "--:--".into()));
    let bar = width.saturating_sub(label.len());
    let filled = duration.filter(|d| !d.is_zero()).map_or(0, |d| ((view.position.as_secs_f64() / d.as_secs_f64()).min(1.0) * bar as f64) as usize);
    let quality = view.selection.map_or_else(|| "format pending".into(), |s| quality(&s, view.source.as_ref()));
    let field = |name: &str, value: String| Line::from(vec![Span::styled(format!("{name:<10}"), p.muted()), Span::raw(value)]);
    let body = vec![
        Line::from(vec![Span::styled(badge, badge_style), Span::raw("  "), Span::styled(title, Style::default().add_modifier(Modifier::BOLD))]),
        Line::styled(detail, p.muted()),
        Line::from(vec![Span::styled("━".repeat(filled), p.accent()), Span::styled("─".repeat(bar - filled), p.muted()), Span::raw(label)]),
        field("Speakers", view.device.clone().unwrap_or_else(|| "none selected".into())),
        field("Quality", quality),
        view.status_line(),
        Line::styled(CONTROLS, p.muted()),
    ];
    let block = Block::bordered().border_style(p.muted()).title(Span::styled(" Now playing ", p.accent().add_modifier(Modifier::BOLD)));
    frame.render_widget(Paragraph::new(body).block(block.padding(ratatui::widgets::Padding::horizontal(1))), now);

    let volume = match view.volume {
        Some(Volume { muted: true, .. }) => "muted".to_string(),
        Some(v) => format!("{:.1} dB", v.db),
        None => "-- dB".into(),
    };
    let hints = view.hints(area.width);
    let [left, right] = Layout::horizontal([Constraint::Min(0), Constraint::Length(Line::raw(hints).width() as u16)]).areas(footer);
    frame.render_widget(Paragraph::new(format!(" {} · {volume} · {}", view.badge().0, view.device.as_deref().unwrap_or("no receiver"))), left);
    frame.render_widget(Paragraph::new(hints).style(p.muted()), right);

    if view.picker.open {
        view.picker.render(frame, p);
    }
    if view.help {
        let height = KEYS.len() as u16 + 2;
        let popup = Rect { x: area.width.saturating_sub(36) / 2, y: area.height.saturating_sub(height) / 2, width: 36.min(area.width), height: height.min(area.height) };
        let lines: Vec<Line> = KEYS.iter().map(|(key, what)| Line::from(vec![Span::styled(format!("{key:>9}  "), p.focus().add_modifier(Modifier::BOLD)), Span::raw(*what)])).collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(p.base()).block(Block::bordered().border_type(BorderType::Double).title(" Keys ")), popup);
    }
}

pub type Restart<'a> = dyn FnMut(Queue) -> Option<Player> + 'a;

fn drive(view: &mut View, deck: &mut Deck, current: Option<&Player>) -> (bool, Option<Player>) {
    let mut dirty = view.poll();
    view.jobs.extend(view.picker.tick(Instant::now()));
    let jobs = std::mem::take(&mut view.jobs);
    if let Some(worker) = &deck.worker {
        jobs.into_iter().for_each(|job| worker.send(job));
        for outcome in worker.poll() {
            if let Some(job) = view.picker.apply(outcome) {
                worker.send(job);
            }
            dirty = true;
        }
    }
    let (switched, play) = (std::mem::take(&mut view.switched), std::mem::take(&mut view.play));
    let active = current.is_some_and(|p| !p.is_finished());
    match (deck.start.as_mut(), view.picker.target()) {
        (Some(start), Some((device, devices))) if (switched && active) || (play && !active) => {
            current.inspect(|p| p.stop());
            (true, Some(start(view.queue_from_current(), device, devices)))
        }
        _ => (dirty, None),
    }
}

pub fn event_loop<B: Backend>(terminal: &mut Terminal<B>, view: &mut View, player: Option<&Player>, restart: &mut Restart, deck: &mut Deck, next: &mut dyn FnMut(Duration) -> io::Result<Option<Event>>) -> anyhow::Result<i32>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (mut last, mut dirty) = (Instant::now() - FRAME, true);
    let mut retried: Option<Player> = None;
    loop {
        if last.elapsed() >= FRAME {
            let (changed, fresh) = drive(view, deck, retried.as_ref().or(player));
            if let Some(fresh) = fresh {
                retried = Some(fresh);
                view.restarted();
            }
            dirty |= changed;
            if let Some(player) = retried.as_ref().or(player) {
                dirty |= view.update(player.status(), player.events());
            }
            if view.animating() {
                view.tick += 1;
                dirty = true;
            }
            if std::mem::take(&mut dirty) {
                terminal.draw(|f| render(f, view))?;
                view.frames += 1;
            }
            last = Instant::now();
        }
        match next(FRAME.saturating_sub(last.elapsed()))? {
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                match view.key(key) {
                    Some(Action::Quit(code)) => return Ok(code),
                    Some(Action::Send(command)) => {
                        if let Some(player) = retried.as_ref().or(player) {
                            view.sent(command, player.send(command));
                        }
                    }
                    Some(Action::Retry(entry) | Action::Play(entry)) => {
                        let from = resume_at(&view.queue, entry);
                        let fresh = match (deck.start.as_mut(), view.picker.target()) {
                            (Some(start), Some((device, devices))) => Some(start(from, device, devices)),
                            _ => restart(from),
                        };
                        match fresh {
                            Some(fresh) => {
                                retried.as_ref().or(player).inspect(|p| p.stop());
                                retried = Some(fresh);
                                view.restarted();
                                view.select(entry);
                            }
                            None => view.note = Some("no receiver: press d to choose one".into()),
                        }
                    }
                    None => {}
                }
                dirty = true;
            }
            Some(Event::Resize(..)) => {
                terminal.autoresize()?;
                dirty = true;
            }
            _ => {}
        }
    }
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore();
    }
}

pub fn session(view: &mut View, player: Option<&Player>, restart: &mut Restart, deck: &mut Deck, next: &mut dyn FnMut(Duration) -> io::Result<Option<Event>>) -> i32 {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let result = terminal::enable_raw_mode().map_err(anyhow::Error::from).and_then(|()| {
        let _restore = Restore;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        event_loop(&mut Terminal::new(CrosstermBackend::new(io::stdout()))?, view, player, restart, deck, next)
    });
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        FAILED
    })
}

pub fn read_terminal(wait: Duration) -> io::Result<Option<Event>> {
    if event::poll(wait)? {
        event::read().map(Some)
    } else {
        Ok(None)
    }
}

pub fn resume_at(queue: &Queue, entry: EntryId) -> Queue {
    let mut from = queue.clone();
    let _ = from.cue(entry);
    from
}

pub fn run(input: &[String]) -> i32 {
    let (mut paths, mut device, mut pin, mut recursive) = (Vec::new(), None, HOMEPOD_PIN.to_string(), false);
    let mut reduced = std::env::var_os("AIRPLAY_REDUCED_MOTION").is_some_and(|v| !v.is_empty());
    let mut it = input.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--device" | "--pin" | "--state-dir" => match (arg.as_str(), it.next()) {
                (_, None) => return (eprintln!("error: {arg} needs a value"), USAGE).1,
                ("--device", Some(v)) => device = Some(v.clone()),
                ("--pin", Some(v)) => pin = v.clone(),
                _ => {}
            },
            "--recursive" => recursive = true,
            "--reduced-motion" => reduced = true,
            a if a.starts_with("--state-dir=") => {}
            a if a.starts_with('-') => return (eprintln!("error: unknown flag {a}"), USAGE).1,
            a => paths.push(std::path::PathBuf::from(a)),
        }
    }
    let mut queue = Queue::new();
    if !paths.is_empty() {
        let scanned = scan(&paths, &ScanOptions { recursive, ..ScanOptions::default() }, &Cancellation::default());
        if let Err(e) = scanned.map_err(|e| e.to_string()).and_then(|r| queue.append(r.tracks).map_err(|e| e.to_string())) {
            return (eprintln!("error: {e}"), USAGE).1;
        }
    }
    let store: Arc<dyn CredentialStore> = match FileStore::open_default() {
        Ok(store) => Arc::new(store),
        Err(e) => return (eprintln!("error: {e}"), FAILED).1,
    };
    let options = SessionOptions { credentials: Some(store.clone()), ..SessionOptions::default() };
    let start = |queue: Queue| device.clone().map(|name| Player::play(queue, discover_device(name, Duration::from_secs(5), pin.clone(), options.clone())));
    let player = start(queue.clone());
    let (chosen_pin, chosen_options) = (pin.clone(), options.clone());
    let chosen = move |from: Queue, target, devices| Player::play(from, connect_device(target, devices, chosen_pin.clone(), chosen_options.clone()));
    let mut deck = Deck { worker: Some(Worker::spawn(crate::auth::Live::new(store.clone()))), start: Some(Box::new(chosen)) };
    let root = paths.first().map(|p| if p.is_dir() { p.clone() } else { p.parent().map_or_else(PathBuf::new, Path::to_path_buf) });
    let root = root.filter(|p| !p.as_os_str().is_empty()).or_else(|| std::env::current_dir().ok());
    let mut view = View::new(None, device.clone(), queue, Palette::detect(|k| std::env::var(k).ok()), reduced);
    (view.picker, view.recursive) = (Picker::new(store), recursive);
    if let Some(root) = root {
        view.open(root);
    }
    session(&mut view, player.as_ref(), &mut |from| start(from), &mut deck, &mut read_terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::apple_tv;
    use crate::play::tests::{write_wav, Fake};
    use airplay_core::audio::{Track, TrackMetadata};
    use airplay_core::{OutputProfile, PlayerError, Position, SessionError, SessionProfile};
    use ratatui::backend::TestBackend;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Stdio;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    fn settle(view: &mut View) {
        let start = Instant::now();
        while view.scan.is_some() {
            view.poll();
            assert!(start.elapsed() < Duration::from_secs(10), "scan did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn press(view: &mut View, codes: &[KeyCode]) {
        for code in codes {
            assert_eq!(view.key(key(*code, KeyModifiers::NONE)), None, "{code:?}");
            settle(view);
        }
    }

    fn screen(view: &View, width: u16, height: u16) -> String {
        draw(view, width, height).backend().to_string()
    }

    fn titles(queue: &Queue) -> Vec<String> {
        queue.entries().iter().map(|e| origin(&e.path)).collect()
    }

    fn library() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["Music/Album/01 Intro.wav", "Music/Album/02 Song.wav", "Music/Album/10 Outro.wav", "Music/Other/02 Song.wav", "Music/loose.wav"] {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_wav(&path, 4_800);
        }
        std::fs::create_dir(dir.path().join("Music/Empty")).unwrap();
        dir
    }

    fn track(title: &str, artist: Option<&str>, album: Option<&str>, seconds: Option<u64>) -> Track {
        let metadata = TrackMetadata { title: title.into(), artist: artist.map(Into::into), album: album.map(Into::into), track: None, duration: seconds.map(Duration::from_secs) };
        Track { path: format!("/music/{title}.flac").into(), metadata }
    }

    fn fixture(state: Option<PlayerState>, palette: Palette) -> View {
        let mut queue = Queue::new();
        queue
            .append(vec![
                track("夜明け前の長い長いプレリュード (Remastered 2026 Edition)", Some("坂本 龍一 & Orchestre Philharmonique de Radio France"), Some("Ｓｈｉｎｊｕｋｕ Ｎｉｇｈｔｓ"), Some(412)),
                track("Ünïcödé Sérénade for a Very Long Afternoon in the Café del Mar", Some("Björk Guðmundsdóttir"), Some("Homogenic (Deluxe Édition)"), Some(225)),
                track("🎧 Lo-fi 🌙 Beats", None, None, None),
            ])
            .unwrap();
        let mut view = View::new(Some("/music/Ｓｈｉｎｊｕｋｕ Ｓｅｓｓｉｏｎｓ".into()), Some("Wohnzimmer HomePod — Küche & Esszimmer".into()), queue, palette, true);
        view.rows = vec![
            Row::Up("/music".into()),
            Row::Folder("/music/Ｓｈｉｎｊｕｋｕ Ｓｅｓｓｉｏｎｓ/Disc 2".into()),
            Row::Track(track("Intro", Some("Björk"), None, Some(61))),
            Row::Track(Track { path: "/music/Ｓｈｉｎｊｕｋｕ Ｓｅｓｓｉｏｎｓ/intro (live).flac".into(), ..track("Intro", None, None, None) }),
            Row::Skipped(Skipped { path: "/music/locked.flac".into(), reason: SkipReason::Unreadable("Permission denied (os error 13)".into()) }),
        ];
        if state.is_some() {
            view.current = view.queue.entries().get(1).map(|e| e.id);
            view.position = Duration::from_secs(83);
            let profile = OutputProfile::of(&SessionOptions::default().profile).unwrap();
            view.selection = Some(ProfileSelection { requested: profile, effective: profile, fallback: None });
            view.source = Some(source("flac", 48_000, Some(24), true));
            view.volume = Some(Volume { db: -30.0, muted: false });
        }
        view.state = state;
        view
    }

    fn source(codec: &str, sample_rate: u32, bits: Option<u32>, lossless: bool) -> SourceFormat {
        SourceFormat { codec: codec.into(), sample_rate, channels: 2, bits_per_sample: bits, lossless }
    }

    fn status(state: PlayerState, entry: EntryId, epoch: u64, seconds: u64) -> PlayerStatus {
        let position = Position { entry: Some(entry), epoch, audible_frames: seconds * 48_000, ..Position::default() };
        PlayerStatus { state, position, volume: Volume { db: -30.0, muted: false }, profile: None, levels: [0.0; 2], dropped_events: 0 }
    }

    fn ids(n: usize) -> Vec<CommandId> {
        let mut queue = Queue::new();
        queue.append(vec![track("held", None, None, Some(1))]).unwrap();
        let player = Player::play(queue, |_: &Cancellation| Ok(Fake { profile: None, hold: true }));
        (0..n).map(|_| player.send(Command::Volume(-10.0)).unwrap()).collect()
    }

    fn text(view: &View) -> String {
        draw(view, 80, 24).backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    fn error() -> Option<PlayerState> {
        Some(PlayerState::Paused { entry: fixture(None, Palette::Rgb).queue.entries()[1].id, error: PlayerError::Session(SessionError::ReceiverUnavailable) })
    }

    fn draw(view: &View, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, view)).unwrap();
        terminal
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn fixture_states_render_at_fixed_sizes() {
        for (name, state, width, height) in [
            ("idle_80x24", None, 80, 24),
            ("connecting_80x24", Some(PlayerState::Connecting), 80, 24),
            ("buffering_80x24", Some(PlayerState::Buffering), 80, 24),
            ("playing_80x24", Some(PlayerState::Playing), 80, 24),
            ("paused_80x24", Some(PlayerState::Held { entry: fixture(None, Palette::Rgb).queue.entries()[1].id }), 80, 24),
            ("ended_80x24", Some(PlayerState::Finished), 80, 24),
            ("failed_80x24", error(), 80, 24),
            ("playing_120x40", Some(PlayerState::Playing), 120, 40),
        ] {
            insta::assert_snapshot!(name, draw(&fixture(state, Palette::Rgb), width, height).backend());
        }
    }

    #[test]
    fn narrow_and_wide_layouts_keep_focused_actions_visible() {
        let mut view = fixture(Some(PlayerState::Playing), Palette::Rgb);
        (view.focus, view.browse_at) = (Pane::Browser, 3);
        insta::assert_snapshot!("browser_50x24", draw(&view, 50, 24).backend());
        for width in [50, 71, 72, 80, 120] {
            for (focus, searching) in [(Pane::Browser, false), (Pane::Queue, false), (Pane::Browser, true)] {
                let mut view = fixture(Some(PlayerState::Playing), Palette::Plain);
                (view.focus, view.searching) = (focus, searching);
                let footer = screen(&view, width, 24).lines().last().unwrap().to_string();
                assert!(footer.contains(view.hints(width).trim_end()), "{width} {focus:?}: {footer}");
            }
        }
    }

    #[test]
    fn input_sequence_browses_enqueues_reorders_removes_and_starts_the_intended_track() {
        use KeyCode::*;
        let dir = library();
        let mut view = View::new(None, None, Queue::new(), Palette::Plain, true);
        view.open(dir.path().join("Music"));
        settle(&mut view);
        assert_eq!(view.rows.iter().map(|r| format!("{r:?}").split('(').next().unwrap().to_string()).collect::<Vec<_>>(), ["Up", "Folder", "Folder", "Folder", "Track"]);
        press(&mut view, &[Tab, Down, Enter, Down, Char('a'), Down, Char('a'), Down, Char('a'), Backspace, Down, Down, Down, Char('a')]);
        assert_eq!(titles(&view.queue), ["Album/01 Intro.wav", "Album/02 Song.wav", "Album/10 Outro.wav", "Other/02 Song.wav"]);
        assert_eq!(view.note.as_deref(), Some("added 1"));
        let text = screen(&view, 120, 30);
        assert!(text.contains("02 Song · Album/02 Song.wav") && text.contains("02 Song · Other/02 Song.wav"), "{text}");
        press(&mut view, &[Tab, Down, Char('x'), Char('K'), Char('K')]);
        assert_eq!(titles(&view.queue), ["Album/10 Outro.wav", "Album/01 Intro.wav", "Other/02 Song.wav"]);
        let Some(Action::Play(id)) = view.key(key(Enter, KeyModifiers::NONE)) else { panic!("Enter did not request playback") };
        let queue = resume_at(&view.queue, id);
        assert_eq!((queue.get(id).map(|e| origin(&e.path)), queue.peek_next()), (Some("Album/10 Outro.wav".into()), Some(id)));
        let player = Player::play(queue, |_: &Cancellation| Ok(Fake { profile: None, hold: false }));
        while !player.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let heard: Vec<EntryId> = player.events().into_iter().filter_map(|e| if let PlayerEvent::TrackAudible(id) = e { Some(id) } else { None }).collect();
        assert_eq!(heard, view.queue.entries().iter().map(|e| e.id).collect::<Vec<_>>());
        press(&mut view, &[Tab, Down, Down, Down, Down]);
        let Some(Action::Play(id)) = view.key(key(Enter, KeyModifiers::NONE)) else { panic!("Enter on a file did not play it") };
        let queue = resume_at(&view.queue, id);
        assert_eq!((titles(&view.queue).last().unwrap().as_str(), view.queue.entries().last().map(|e| e.id)), ("Music/loose.wav", Some(id)));
        assert_eq!(queue.peek_next(), Some(id));
    }

    #[test]
    fn empty_unreadable_long_and_duplicate_entries_show_useful_states() {
        use KeyCode::*;
        let dir = library();
        let music = dir.path().join("Music");
        let long = "A very long track name that keeps going well past any terminal width you might reasonably use.wav";
        write_wav(&music.join(long), 4_800);
        std::fs::write(music.join("broken.flac"), b"not audio").unwrap();
        write_wav(&music.join("locked.wav"), 4_800);
        std::fs::set_permissions(music.join("locked.wav"), std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::create_dir(music.join("Sealed")).unwrap();
        std::fs::set_permissions(music.join("Sealed"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut view = View::new(None, None, Queue::new(), Palette::Plain, true);
        view.focus = Pane::Browser;
        view.open(music.clone());
        settle(&mut view);
        let text = screen(&view, 160, 30);
        assert!(text.contains("✕ locked.wav unreadable: Permission denied"), "{text}");
        assert!(text.contains("✕ broken.flac unsupported:") && text.contains("▹ Sealed/"), "{text}");
        let narrow = screen(&view, 80, 24);
        assert!(narrow.contains("   A very long track") && !narrow.contains("reasonably") && narrow.contains("▸ Browse Music"), "{narrow}");
        let at = view.visible().iter().position(|r| matches!(r, Row::Skipped(s) if s.path.ends_with("locked.wav"))).unwrap();
        view.browse_at = at;
        press(&mut view, &[Char('a')]);
        assert!(view.queue.is_empty() && view.note.as_deref().is_some_and(|n| n.starts_with("unreadable")));
        press(&mut view, &[Char('/'), Char('z'), Char('z'), Enter]);
        assert!(screen(&view, 80, 24).contains("no matches for “zz”"));
        press(&mut view, &[Char('/'), Char('s'), Char('e'), Char('a'), Enter, Down, Enter]);
        let text = screen(&view, 120, 24);
        std::fs::set_permissions(music.join("Sealed"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(text.contains("Browse Sealed") && text.contains("✕ Sealed unreadable: Permission denied"), "{text}");
        press(&mut view, &[Backspace, Down, Down, Enter]);
        assert!(screen(&view, 80, 24).contains("empty folder · no music here"));
        view.recursive = true;
        press(&mut view, &[Backspace]);
        view.browse_at = 1;
        view.key(key(Char('a'), KeyModifiers::NONE));
        assert_eq!(view.key(key(Esc, KeyModifiers::NONE)), None);
        assert!(view.scan.is_none() && view.note.as_deref().is_some_and(|n| n.starts_with("cancelled")), "{:?}", view.note);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!view.poll() && view.queue.is_empty());
        press(&mut view, &[Char('a')]);
        assert_eq!(titles(&view.queue), ["Album/01 Intro.wav", "Album/02 Song.wav", "Album/10 Outro.wav"]);
    }

    #[test]
    fn queue_edits_around_the_active_track_keep_identity_and_next_track() {
        use KeyCode::*;
        let mut view = fixture(Some(PlayerState::Playing), Palette::Plain);
        let ids: Vec<EntryId> = view.queue.entries().iter().map(|e| e.id).collect();
        view.queue.select(ids[1]).unwrap();
        view.queue_at = Some(ids[2]);
        press(&mut view, &[Char('K'), Char('K')]);
        assert_eq!(view.queue.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [ids[2], ids[0], ids[1]]);
        assert_eq!((view.current, view.queue.current().map(|e| e.id), view.queue.peek_next()), (Some(ids[1]), Some(ids[1]), None));
        assert!(view.stale && screen(&view, 120, 24).contains("edits apply on next play"));
        view.queue_at = Some(ids[1]);
        view.key(key(Up, KeyModifiers::SHIFT));
        assert_eq!((view.queue.peek_next(), view.queue.peek_previous(), view.queue_at), (Some(ids[0]), Some(ids[2]), Some(ids[1])));
        press(&mut view, &[Char('x')]);
        assert_eq!((view.queue.len(), view.note.as_deref()), (3, Some("the playing track stays queued")));
        press(&mut view, &[Char('c')]);
        assert_eq!((view.queue.entries().iter().map(|e| e.id).collect::<Vec<_>>(), view.queue.current().map(|e| e.id)), (vec![ids[1]], Some(ids[1])));
        view.current = None;
        press(&mut view, &[Char('c')]);
        assert!(view.queue.is_empty() && view.queue_at.is_none());
    }

    #[test]
    fn no_color_keeps_focus_and_playback_cues() {
        for state in [Some(PlayerState::Playing), error(), Some(PlayerState::Buffering)] {
            let mut view = fixture(state, Palette::Plain);
            let terminal = draw(&view, 80, 24);
            let buffer = terminal.backend().buffer();
            assert!(buffer.content.iter().all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
            let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("▸ Queue") && text.contains("┏") && !text.contains("▸ Browse"));
            assert!(["▶ PLAYING", "✕ FAILED", "… BUFFERING"].iter().any(|cue| text.contains(cue)), "{text}");
            assert!(text.contains("▶  2."));
            view.key(key(KeyCode::Tab, KeyModifiers::NONE));
            let text: String = draw(&view, 80, 24).backend().buffer().content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("▸ Browse") && !text.contains("▸ Queue"));
        }
    }

    #[test]
    fn palette_follows_no_color_and_terminal_depth() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(Palette::detect(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])), Palette::Plain);
        assert_eq!(Palette::detect(env(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])), Palette::Rgb);
        assert_eq!(Palette::detect(env(&[("TERM", "xterm-256color")])), Palette::Indexed);
        assert_eq!(Palette::detect(env(&[("TERM", "vt100")])), Palette::Basic);
    }

    #[test]
    fn keys_toggle_help_and_focus_and_quit() {
        let mut view = fixture(None, Palette::Basic);
        assert_eq!(view.key(key(KeyCode::Char('?'), KeyModifiers::NONE)), None);
        assert!(draw(&view, 80, 40).backend().to_string().contains("move focus"));
        assert_eq!((view.key(key(KeyCode::Esc, KeyModifiers::NONE)), view.help), (None, false));
        assert_eq!((view.key(key(KeyCode::BackTab, KeyModifiers::SHIFT)), view.focus), (None, Pane::Browser));
        assert_eq!(view.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Quit(CANCELLED)));
        assert_eq!(view.key(key(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit(0)));
    }

    #[test]
    fn transport_keys_map_to_bounded_player_commands() {
        let mut view = fixture(Some(PlayerState::Playing), Palette::Basic);
        let press = |view: &mut View, code| view.key(key(code, KeyModifiers::NONE));
        let send = |c| Some(Action::Send(c));
        assert_eq!(press(&mut view, KeyCode::Char(' ')), send(Command::Pause));
        assert_eq!(press(&mut view, KeyCode::Right), send(Command::Seek(Duration::from_secs(93))));
        assert_eq!(press(&mut view, KeyCode::Left), send(Command::Seek(Duration::from_secs(73))));
        assert_eq!(press(&mut view, KeyCode::Char('n')), send(Command::Next));
        assert_eq!(press(&mut view, KeyCode::Char('p')), send(Command::Previous));
        assert_eq!(press(&mut view, KeyCode::Char('s')), send(Command::Stop));
        assert_eq!(press(&mut view, KeyCode::Char('m')), send(Command::Mute(true)));
        assert_eq!(press(&mut view, KeyCode::Char('+')), send(Command::Volume(-28.0)));
        assert_eq!(press(&mut view, KeyCode::Char('r')), None);
        view.position = Duration::from_secs(220);
        assert_eq!(press(&mut view, KeyCode::Right), send(Command::Seek(Duration::from_secs(225))));
        view.position = Duration::from_secs(4);
        assert_eq!(press(&mut view, KeyCode::Left), send(Command::Seek(Duration::ZERO)));
        view.volume = Some(Volume { db: -1.0, muted: true });
        assert_eq!((press(&mut view, KeyCode::Char('+')), press(&mut view, KeyCode::Char('m'))), (send(Command::Volume(0.0)), send(Command::Mute(false))));
        view.volume = Some(Volume { db: 0.0, muted: false });
        assert_eq!(press(&mut view, KeyCode::Char('=')), None);
        view.volume = Some(Volume { db: -143.0, muted: false });
        assert_eq!(press(&mut view, KeyCode::Char('-')), send(Command::Volume(MIN_VOLUME_DB)));
        view.volume = Some(Volume { db: MIN_VOLUME_DB, muted: false });
        assert_eq!(press(&mut view, KeyCode::Char('-')), None);
        view.state = Some(PlayerState::Held { entry: view.queue.entries()[1].id });
        assert_eq!(press(&mut view, KeyCode::Char(' ')), send(Command::Resume));
        for state in [PlayerState::Finished, PlayerState::Stopped] {
            view.state = Some(state);
            assert_eq!(press(&mut view, KeyCode::Char(' ')), None);
        }
        view.state = error();
        assert_eq!(press(&mut view, KeyCode::Char('r')), Some(Action::Retry(view.queue.entries()[1].id)));
        let mut queue = Queue::new();
        queue.append(vec![track("a", None, None, None), track("b", None, None, None)]).unwrap();
        let [a, b] = [queue.entries()[0].id, queue.entries()[1].id];
        assert_eq!((resume_at(&queue, a).peek_next(), resume_at(&queue, b).peek_next()), (Some(a), Some(b)));
    }

    #[test]
    fn pending_controls_stay_visible_until_the_player_completes_them() {
        let mut view = fixture(Some(PlayerState::Playing), Palette::Plain);
        let id = ids(4);
        view.sent(Command::Pause, Ok(id[0]));
        view.sent(Command::Seek(Duration::from_secs(93)), Ok(id[1]));
        view.sent(Command::Volume(-28.0), Ok(id[2]));
        assert_eq!(view.key(key(KeyCode::Right, KeyModifiers::NONE)), Some(Action::Send(Command::Seek(Duration::from_secs(103)))));
        assert_eq!(view.key(key(KeyCode::Char('+'), KeyModifiers::NONE)), Some(Action::Send(Command::Volume(-26.0))));
        let screen = text(&view);
        assert!(screen.contains("▶ PLAYING") && screen.contains("⋯ pausing · seeking to 1:33 · volume -28.0 dB"), "{screen}");
        let entry = view.queue.entries()[1].id;
        view.update(status(PlayerState::Pausing { entry }, entry, 1, 83), vec![PlayerEvent::Command { id: id[2], state: CommandState::Completed }]);
        assert!(text(&view).contains("PAUSING") && !text(&view).contains("PAUSED"));
        let done = |id, state| PlayerEvent::Command { id, state };
        view.update(status(PlayerState::Held { entry }, entry, 1, 83), vec![done(id[0], CommandState::Completed), done(id[1], CommandState::Failed(CommandError::Busy))]);
        let screen = text(&view);
        assert!(screen.contains("‖ PAUSED") && screen.contains("✕ too many pending commands") && !screen.contains("⋯"), "{screen}");
        view.sent(Command::Next, Err(CommandError::Stopped));
        assert!(text(&view).contains("✕ player has stopped"));
    }

    #[test]
    fn stale_seek_completion_cannot_update_the_next_track() {
        let mut view = fixture(Some(PlayerState::Playing), Palette::Plain);
        let ([first, second], id) = ([view.queue.entries()[1].id, view.queue.entries()[2].id], ids(2));
        view.update(status(PlayerState::Playing, first, 4, 83), Vec::new());
        view.sent(Command::Seek(Duration::from_secs(200)), Ok(id[0]));
        view.sent(Command::Seek(Duration::from_secs(210)), Ok(id[1]));
        assert!(text(&view).contains("seeking to 3:30"));
        view.update(status(PlayerState::Playing, second, 5, 2), vec![PlayerEvent::TrackAudible(second)]);
        assert!(!text(&view).contains("seeking"));
        let stale = vec![PlayerEvent::Command { id: id[0], state: CommandState::Completed }, PlayerEvent::Command { id: id[1], state: CommandState::Failed(CommandError::NoEntry) }];
        view.update(status(PlayerState::Playing, first, 4, 200), stale);
        let screen = text(&view);
        assert_eq!((view.current, view.position, view.epoch, view.notice.clone()), (Some(second), Duration::from_secs(2), 5, None));
        assert!(screen.contains("▶  3.") && screen.contains("est. 0:02") && !screen.contains("3:20"), "{screen}");
    }

    #[test]
    fn effective_profile_never_overstates_provenance() {
        let buffered = OutputProfile::of(&SessionOptions::default().profile).unwrap();
        let realtime = OutputProfile::of(&SessionProfile::Realtime { sample_rate: 44_100 }).unwrap();
        let pick = |effective| ProfileSelection { requested: buffered, effective, fallback: (effective != buffered).then_some(airplay_core::FallbackReason::NotAdvertised) };
        let native = source("flac", 48_000, Some(24), true);
        for (effective, src, expect) in [
            (buffered, Some(native.clone()), "PCM 24-bit/48 kHz · buffered · native lossless"),
            (realtime, Some(native.clone()), "PCM 16-bit/44.1 kHz · realtime · converted from FLAC 24-bit/48 kHz · fallback from PCM"),
            (buffered, Some(source("mp3", 48_000, None, false)), "PCM 24-bit/48 kHz · buffered · lossy MP3 source"),
            (buffered, Some(source("aac", 44_100, None, false)), "PCM 24-bit/48 kHz · buffered · lossy AAC source"),
            (buffered, Some(source("flac", 48_000, Some(16), true)), "PCM 24-bit/48 kHz · buffered · lossless from FLAC 16-bit/48 kHz"),
            (buffered, Some(source("pcm_s16le", 44_100, Some(16), true)), "PCM 24-bit/48 kHz · buffered · converted from PCM 16-bit/44.1 kHz"),
            (buffered, None, "PCM 24-bit/48 kHz · buffered · source unknown"),
        ] {
            let label = quality(&pick(effective), src.as_ref());
            assert_eq!(label, expect);
            assert!(!label.contains("native") || (effective.is_buffered_lossless_24_48() && src == Some(native.clone())));
        }
    }

    #[test]
    fn level_meter_follows_library_levels_only_while_playing() {
        let mut view = fixture(Some(PlayerState::Playing), Palette::Plain);
        view.reduced_motion = false;
        let entry = view.queue.entries()[1].id;
        let mut playing = status(PlayerState::Playing, entry, 1, 83);
        playing.levels = [1.0, 0.004];
        assert!(view.update(playing.clone(), Vec::new()));
        assert_eq!(view.levels, [8, 0]);
        assert!(!view.update(playing, Vec::new()));
        assert!(view.source.is_none());
        assert!(text(&view).contains("L ████████  R ░░░░░░░░"));
        view.update(status(PlayerState::Held { entry }, entry, 1, 83), Vec::new());
        assert!(!text(&view).contains("L █"));
    }

    #[test]
    fn resizes_and_progress_floods_do_not_block_the_player_worker() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(&dir.path().join("a.wav"), 48_000 * 3);
        let mut queue = Queue::new();
        queue.append(scan(&[dir.path().to_path_buf()], &ScanOptions::default(), &Cancellation::default()).unwrap().tracks).unwrap();
        let player = Player::play(queue.clone(), |_: &Cancellation| Ok(Fake { profile: None, hold: false }));
        let mut view = View::new(None, Some("test".into()), queue.clone(), Palette::Rgb, false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (start, mut resizes) = (Instant::now(), 0u64);
        let code = event_loop(&mut terminal, &mut view, Some(&player), &mut |_| None, &mut Deck { worker: None, start: None }, &mut |_| {
            assert!(start.elapsed() < Duration::from_secs(20), "player worker did not finish");
            resizes += 1;
            if player.is_finished() && resizes > 1_000 {
                return Ok(Some(Event::Key(key(KeyCode::Char('q'), KeyModifiers::NONE))));
            }
            Ok(Some(Event::Resize(80, 24)))
        });
        assert_eq!(code.unwrap(), 0);
        let elapsed = start.elapsed();
        assert_eq!(player.wait().state, PlayerState::Finished);
        assert!(view.frames <= (elapsed.as_millis() / FRAME.as_millis()) as u64 + 2, "{} frames in {elapsed:?}", view.frames);
        assert!(resizes > view.frames * 10);
    }

    #[test]
    fn selecting_waits_for_play_and_switching_restarts_on_the_new_receiver() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(&dir.path().join("a.wav"), 48_000);
        let mut queue = Queue::new();
        queue.append(scan(&[dir.path().to_path_buf()], &ScanOptions::default(), &Cancellation::default()).unwrap().tracks).unwrap();
        let mut view = View::new(None, None, queue.clone(), Palette::Plain, true);
        view.picker.apply(Outcome::Devices(Ok(vec![apple_tv("aa:01", "Den"), apple_tv("aa:02", "Kitchen")])));
        let started = Arc::new(Mutex::new(Vec::new()));
        let log = started.clone();
        let start = move |_: Queue, device: Device, _: Vec<Device>| {
            log.lock().unwrap().push(device.record.device_id);
            Player::play(queue.clone(), |_: &Cancellation| Ok(Fake { profile: None, hold: true }))
        };
        let mut deck = Deck { worker: None, start: Some(Box::new(start)) };
        let mut script = vec![Some('d'), Some('\n'), None, None, Some(' '), None, None, Some('d'), Some('j'), Some('\n'), None, None, Some('q')].into_iter();
        let (mut terminal, seen) = (Terminal::new(TestBackend::new(80, 24)).unwrap(), started.clone());
        let code = event_loop(&mut terminal, &mut view, None, &mut |_| None, &mut deck, &mut |wait| {
            let step = script.next().flatten();
            if step == Some(' ') {
                assert!(seen.lock().unwrap().is_empty(), "selecting a receiver started playback");
            }
            Ok(match step {
                None => (std::thread::sleep(wait + FRAME), None).1,
                Some('\n') => Some(Event::Key(key(KeyCode::Enter, KeyModifiers::NONE))),
                Some(c) => Some(Event::Key(key(KeyCode::Char(c), KeyModifiers::NONE))),
            })
        });
        assert_eq!(code.unwrap(), 0);
        assert_eq!(*started.lock().unwrap(), ["aa:01", "aa:02"]);
        assert_eq!(view.device.as_deref(), Some("Kitchen (aa:02)"));
    }

    #[test]
    #[ignore]
    fn pty_child() {
        let Ok(mode) = std::env::var("AIRPLAY_TUI_PTY") else { return };
        let mut view = View::new(None, None, Queue::new(), Palette::Basic, true);
        std::process::exit(match mode.as_str() {
            "run" => run(&[]),
            "error" => session(&mut view, None, &mut |_| None, &mut Deck { worker: None, start: None }, &mut |_| Err(io::Error::other("simulated terminal failure"))),
            _ => session(&mut view, None, &mut |_| None, &mut Deck { worker: None, start: None }, &mut |_| panic!("simulated panic")),
        });
    }

    fn pty(mode: &str, input: &[u8]) -> (Option<i32>, String, bool) {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) }, 0);
        let (master, slave) = unsafe { (std::fs::File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let termios = || {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut t) }, 0);
            t.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG)
        };
        let before = termios();
        let stdio = || Stdio::from(slave.try_clone().unwrap());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "tui::tests::pty_child", "--nocapture"])
            .env("AIRPLAY_TUI_PTY", mode)
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let (sink, mut reader) = (output.clone(), master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let text = || String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !text().contains("\x1b[?1049h") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        (&master).write_all(input).unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{mode}: child did not exit: {:?}", text());
            std::thread::sleep(Duration::from_millis(10));
        };
        std::thread::sleep(Duration::from_millis(100));
        (status.code(), text(), termios() == before)
    }

    #[test]
    fn pty_sessions_restore_the_terminal_on_quit_interrupt_error_and_panic() {
        for (mode, input, code, message) in [("run", &b"q"[..], 0, "IDLE"), ("run", b"\x03", CANCELLED, "IDLE"), ("error", b"", FAILED, "simulated terminal failure"), ("panic", b"", 101, "simulated panic")] {
            let (status, out, termios) = pty(mode, input);
            let entered = out.rfind("\x1b[?1049h").unwrap_or(usize::MAX);
            let restored = out.rfind("\x1b[?1049l").is_some_and(|i| i > entered) && out.rfind("\x1b[?25h").is_some_and(|i| i > entered);
            assert_eq!((status, termios, restored), (Some(code), true, true), "{mode} {input:?}: {out:?}");
            assert!(out.contains(message), "{mode}: {out:?}");
        }
    }
}
