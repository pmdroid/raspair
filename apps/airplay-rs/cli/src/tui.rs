use crate::play::{CANCELLED, FAILED, USAGE};
use airplay_core::audio::{scan, EntryId, Queue, QueueEntry, ScanOptions};
use airplay_core::pairing::{CredentialStore, FileStore};
use airplay_core::player::discover_device;
use airplay_core::protocol::HOMEPOD_PIN;
use airplay_core::{Cancellation, Player, PlayerEvent, PlayerState, PlayerStatus, SessionOptions, Volume};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const FRAME: Duration = Duration::from_millis(50);
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const KEYS: [(&str, &str); 4] = [("Tab", "move focus"), ("?", "toggle help"), ("q / Esc", "quit"), ("Ctrl-C", "quit")];

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
    Sources,
    Queue,
}

pub struct View {
    pub sources: Vec<String>,
    pub device: Option<String>,
    pub queue: Vec<QueueEntry>,
    pub state: Option<PlayerState>,
    pub current: Option<EntryId>,
    pub position: Duration,
    pub profile: Option<String>,
    pub volume: Option<Volume>,
    pub focus: Pane,
    pub help: bool,
    pub palette: Palette,
    pub reduced_motion: bool,
    pub tick: usize,
    pub frames: u64,
}

impl View {
    pub fn new(sources: Vec<String>, device: Option<String>, queue: Vec<QueueEntry>, palette: Palette, reduced_motion: bool) -> Self {
        let current = queue.first().map(|e| e.id);
        Self { sources, device, queue, state: None, current, position: Duration::ZERO, profile: None, volume: None, focus: Pane::Queue, help: false, palette, reduced_motion, tick: 0, frames: 0 }
    }

    pub fn update(&mut self, status: PlayerStatus, events: Vec<PlayerEvent>) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                PlayerEvent::State(state) => self.state = Some(state),
                PlayerEvent::TrackAudible(id) => self.current = Some(id),
                _ => continue,
            }
            changed = true;
        }
        let position = Duration::from_millis(status.position.audible_frames * 1000 / 48_000);
        let profile = status.profile.map(|s| s.effective.to_string());
        if (position.as_secs(), &profile, Some(status.volume)) != (self.position.as_secs(), &self.profile, self.volume) {
            (self.position, self.profile, self.volume, changed) = (position, profile, Some(status.volume), true);
        }
        changed
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<i32> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Some(CANCELLED),
            KeyCode::Esc if self.help => self.help = false,
            KeyCode::Char('q') | KeyCode::Esc => return Some(0),
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Tab | KeyCode::BackTab => self.focus = if self.focus == Pane::Queue { Pane::Sources } else { Pane::Queue },
            _ => {}
        }
        None
    }

    fn animating(&self) -> bool {
        !self.reduced_motion && matches!(self.state, Some(PlayerState::Connecting | PlayerState::Buffering))
    }

    fn badge(&self) -> (String, Style) {
        let p = self.palette;
        let spin = if self.reduced_motion { "…" } else { SPINNER[self.tick / 4 % 4] };
        let (text, style) = match &self.state {
            None => ("○ IDLE".into(), p.muted()),
            Some(PlayerState::Connecting) => (format!("{spin} CONNECTING"), p.warm()),
            Some(PlayerState::Buffering) => (format!("{spin} BUFFERING"), p.warm()),
            Some(PlayerState::Playing) => ("▶ PLAYING".into(), p.accent()),
            Some(PlayerState::Pausing { .. } | PlayerState::Held { .. }) => ("‖ PAUSED".into(), p.warm()),
            Some(PlayerState::Finished) => ("✓ FINISHED".into(), p.muted()),
            Some(PlayerState::Stopped) => ("■ STOPPED".into(), p.muted()),
            Some(PlayerState::Paused { .. }) => ("✕ ERROR".into(), p.error()),
        };
        (text, style.add_modifier(Modifier::BOLD))
    }

    fn pane(&self, pane: Pane, title: &str) -> Block<'static> {
        let p = self.palette;
        let (marker, border, kind) = match self.focus == pane {
            true => ("▸ ", p.focus().add_modifier(Modifier::BOLD), BorderType::Thick),
            false => ("", p.muted(), BorderType::Plain),
        };
        Block::bordered().border_type(kind).border_style(border).title(Span::styled(format!(" {marker}{title} "), border))
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn render(frame: &mut Frame, view: &View) {
    let p = view.palette;
    let area = frame.area();
    frame.render_widget(Block::new().style(p.base()), area);
    let [top, now, footer] = Layout::vertical([Constraint::Min(5), Constraint::Length(6), Constraint::Length(1)]).areas(area);
    let [sources, queue] = Layout::horizontal([Constraint::Percentage(30), Constraint::Min(20)]).areas(top);

    let mut lines: Vec<Line> = vec![Line::styled("Library", p.muted())];
    lines.extend(view.sources.iter().map(|s| Line::raw(s.clone())));
    if view.sources.is_empty() {
        lines.push(Line::styled("no folders", p.muted()));
    }
    lines.extend([Line::raw(""), Line::styled("Receiver", p.muted()), Line::raw(view.device.clone().unwrap_or_else(|| "none (--device)".into()))]);
    frame.render_widget(Paragraph::new(lines).block(view.pane(Pane::Sources, "Sources")), sources);

    let rows: Vec<Line> = view
        .queue
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let playing = view.current == Some(e.id);
            let length = e.metadata.duration.map(clock).unwrap_or_else(|| "--:--".into());
            let artist = e.metadata.artist.as_ref().map(|a| format!(" · {a}")).unwrap_or_default();
            let style = if playing { p.accent().add_modifier(Modifier::BOLD) } else { Style::default() };
            Line::from(vec![Span::styled(format!("{}{:>3}. {length:>6}  {}", if playing { "▶" } else { " " }, i + 1, e.metadata.title), style), Span::styled(artist, p.muted())])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows).block(view.pane(Pane::Queue, &format!("Queue ({})", view.queue.len()))), queue);

    let entry = view.current.and_then(|id| view.queue.iter().find(|e| e.id == id));
    let (badge, badge_style) = view.badge();
    let title = entry.map_or("Nothing queued".to_string(), |e| e.metadata.title.clone());
    let detail = entry.map(|e| [e.metadata.artist.clone(), e.metadata.album.clone()].into_iter().flatten().collect::<Vec<_>>().join(" — ")).unwrap_or_default();
    let width = now.width.saturating_sub(4) as usize;
    let duration = entry.and_then(|e| e.metadata.duration);
    let label = format!(" {} / {}", clock(view.position), duration.map(clock).unwrap_or_else(|| "--:--".into()));
    let bar = width.saturating_sub(label.len());
    let filled = duration.filter(|d| !d.is_zero()).map_or(0, |d| ((view.position.as_secs_f64() / d.as_secs_f64()).min(1.0) * bar as f64) as usize);
    let info = match &view.state {
        Some(PlayerState::Paused { error, .. }) => Line::styled(error.to_string(), p.error()),
        _ => Line::styled(view.profile.clone().unwrap_or_else(|| "format pending".into()), p.muted()),
    };
    let body = vec![
        Line::from(vec![Span::styled(badge, badge_style), Span::raw("  "), Span::styled(title, Style::default().add_modifier(Modifier::BOLD))]),
        Line::styled(detail, p.muted()),
        Line::from(vec![Span::styled("━".repeat(filled), p.accent()), Span::styled("─".repeat(bar - filled), p.muted()), Span::raw(label)]),
        info,
    ];
    let block = Block::bordered().border_style(p.muted()).title(Span::styled(" Now playing ", p.accent().add_modifier(Modifier::BOLD)));
    frame.render_widget(Paragraph::new(body).block(block.padding(ratatui::widgets::Padding::horizontal(1))), now);

    let volume = match view.volume {
        Some(Volume { muted: true, .. }) => "muted".to_string(),
        Some(v) => format!("{:.1} dB", v.db),
        None => "-- dB".into(),
    };
    let hints = "  Tab focus  ? help  q quit ";
    let [left, right] = Layout::horizontal([Constraint::Min(0), Constraint::Length(hints.len() as u16)]).areas(footer);
    frame.render_widget(Paragraph::new(format!(" {} · {volume} · {}", view.badge().0, view.device.as_deref().unwrap_or("no receiver"))), left);
    frame.render_widget(Paragraph::new(hints).style(p.muted()), right);

    if view.help {
        let popup = Rect { x: area.width.saturating_sub(36) / 2, y: area.height.saturating_sub(8) / 2, width: 36.min(area.width), height: 8.min(area.height) };
        let lines: Vec<Line> = KEYS.iter().map(|(key, what)| Line::from(vec![Span::styled(format!("{key:>9}  "), p.focus().add_modifier(Modifier::BOLD)), Span::raw(*what)])).collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(p.base()).block(Block::bordered().border_type(BorderType::Double).title(" Keys ")), popup);
    }
}

pub fn event_loop<B: Backend>(terminal: &mut Terminal<B>, view: &mut View, player: Option<&Player>, next: &mut dyn FnMut(Duration) -> io::Result<Option<Event>>) -> anyhow::Result<i32>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (mut last, mut dirty) = (Instant::now() - FRAME, true);
    loop {
        if last.elapsed() >= FRAME {
            if let Some(player) = player {
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
                if let Some(code) = view.key(key) {
                    return Ok(code);
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

pub fn session(view: &mut View, player: Option<&Player>, next: &mut dyn FnMut(Duration) -> io::Result<Option<Event>>) -> i32 {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let result = terminal::enable_raw_mode().map_err(anyhow::Error::from).and_then(|()| {
        let _restore = Restore;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        event_loop(&mut Terminal::new(CrosstermBackend::new(io::stdout()))?, view, player, next)
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
    if device.is_some() && paths.is_empty() {
        return (eprintln!("error: --device needs one or more PATH arguments to play"), USAGE).1;
    }
    let mut queue = Queue::new();
    if !paths.is_empty() {
        let scanned = scan(&paths, &ScanOptions { recursive, ..ScanOptions::default() }, &Cancellation::default());
        if let Err(e) = scanned.map_err(|e| e.to_string()).and_then(|r| queue.append(r.tracks).map_err(|e| e.to_string())) {
            return (eprintln!("error: {e}"), USAGE).1;
        }
    }
    let player = match device.clone().map(|name| FileStore::open_default().map(|store| (name, store))).transpose() {
        Ok(found) => found.map(|(name, store)| {
            let options = SessionOptions { credentials: Some(Arc::new(store) as Arc<dyn CredentialStore>), ..SessionOptions::default() };
            Player::play(queue.clone(), discover_device(name, Duration::from_secs(5), pin, options))
        }),
        Err(e) => return (eprintln!("error: {e}"), FAILED).1,
    };
    let sources = paths.iter().map(|p| p.display().to_string()).collect();
    let mut view = View::new(sources, device, queue.entries().to_vec(), Palette::detect(|k| std::env::var(k).ok()), reduced);
    session(&mut view, player.as_ref(), &mut read_terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::play::tests::{write_wav, Fake};
    use airplay_core::audio::{Track, TrackMetadata};
    use airplay_core::{OutputProfile, PlayerError, SessionError};
    use ratatui::backend::TestBackend;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

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
        let sources = vec!["~/Music/Ｓｈｉｎｊｕｋｕ Ｓｅｓｓｉｏｎｓ".into(), "~/Music/Björk – Homogenic".into()];
        let mut view = View::new(sources, Some("Wohnzimmer HomePod — Küche & Esszimmer".into()), queue.entries().to_vec(), palette, true);
        if state.is_some() {
            view.current = view.queue.get(1).map(|e| e.id);
            view.position = Duration::from_secs(83);
            view.profile = Some(OutputProfile::of(&SessionOptions::default().profile).unwrap().to_string());
            view.volume = Some(Volume { db: -30.0, muted: false });
        }
        view.state = state;
        view
    }

    fn error() -> Option<PlayerState> {
        Some(PlayerState::Paused { entry: fixture(None, Palette::Rgb).queue[1].id, error: PlayerError::Session(SessionError::ReceiverUnavailable) })
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
            ("buffering_80x24", Some(PlayerState::Buffering), 80, 24),
            ("playing_80x24", Some(PlayerState::Playing), 80, 24),
            ("error_80x24", error(), 80, 24),
            ("playing_120x40", Some(PlayerState::Playing), 120, 40),
        ] {
            insta::assert_snapshot!(name, draw(&fixture(state, Palette::Rgb), width, height).backend());
        }
    }

    #[test]
    fn no_color_keeps_focus_and_playback_cues() {
        for state in [Some(PlayerState::Playing), error(), Some(PlayerState::Buffering)] {
            let mut view = fixture(state, Palette::Plain);
            let terminal = draw(&view, 80, 24);
            let buffer = terminal.backend().buffer();
            assert!(buffer.content.iter().all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
            let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("▸ Queue") && text.contains("┏") && !text.contains("▸ Sources"));
            assert!(["▶ PLAYING", "✕ ERROR", "… BUFFERING"].iter().any(|cue| text.contains(cue)), "{text}");
            assert!(text.contains("▶  2."));
            view.key(key(KeyCode::Tab, KeyModifiers::NONE));
            let text: String = draw(&view, 80, 24).backend().buffer().content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("▸ Sources") && !text.contains("▸ Queue"));
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
        assert!(draw(&view, 80, 24).backend().to_string().contains("move focus"));
        assert_eq!((view.key(key(KeyCode::Esc, KeyModifiers::NONE)), view.help), (None, false));
        assert_eq!((view.key(key(KeyCode::BackTab, KeyModifiers::SHIFT)), view.focus), (None, Pane::Sources));
        assert_eq!(view.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(CANCELLED));
        assert_eq!(view.key(key(KeyCode::Char('q'), KeyModifiers::NONE)), Some(0));
    }

    #[test]
    fn resizes_and_progress_floods_do_not_block_the_player_worker() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(&dir.path().join("a.wav"), 48_000 * 3);
        let mut queue = Queue::new();
        queue.append(scan(&[dir.path().to_path_buf()], &ScanOptions::default(), &Cancellation::default()).unwrap().tracks).unwrap();
        let player = Player::play(queue.clone(), |_: &Cancellation| Ok(Fake { profile: None, hold: false }));
        let mut view = View::new(Vec::new(), Some("test".into()), queue.entries().to_vec(), Palette::Rgb, false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (start, mut resizes) = (Instant::now(), 0u64);
        let code = event_loop(&mut terminal, &mut view, Some(&player), &mut |_| {
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
    #[ignore]
    fn pty_child() {
        let Ok(mode) = std::env::var("AIRPLAY_TUI_PTY") else { return };
        let mut view = View::new(Vec::new(), None, Vec::new(), Palette::Basic, true);
        std::process::exit(match mode.as_str() {
            "run" => run(&[]),
            "error" => session(&mut view, None, &mut |_| Err(io::Error::other("simulated terminal failure"))),
            _ => session(&mut view, None, &mut |_| panic!("simulated panic")),
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
        let mut child = Command::new(std::env::current_exe().unwrap())
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
