use crate::audio::{AudioReader, Converter, EntryId, OutputFormat, Queue, Samples, SourceFormat};
use crate::discovery::{discover_until, find_device};
use crate::profile::ProfileSelection;
use crate::stream::{buffered_queue, BufferedFeed, BufferedProducer, BufferedProgress};
use crate::{Cancellation, Device, Session, SessionError, SessionOptions, StereoPair};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const OUTPUT_RATE: u32 = 48_000;
pub const EVENT_CAPACITY: usize = 64;
pub const COMMAND_CAPACITY: usize = 16;
pub const STARTUP_VOLUME_DB: f64 = -30.0;
pub const MIN_VOLUME_DB: f64 = -144.0;
const POLL: Duration = Duration::from_millis(10);
const LEVEL_CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayerError {
    #[error("cannot decode {path:?}: {message}")]
    Decode { path: PathBuf, message: String },
    #[error("playback session failed: {0}")]
    Session(SessionError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerState {
    Connecting,
    Buffering,
    Playing,
    Pausing { entry: EntryId },
    Held { entry: EntryId },
    Finished,
    Stopped,
    Paused { entry: EntryId, error: PlayerError },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub entry: Option<EntryId>,
    pub epoch: u64,
    pub submitted_frames: u64,
    pub audible_frames: u64,
    pub receiver_buffered_frames: u64,
    pub local_queued_frames: u64,
    pub peak_local_queued_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Pause,
    Resume,
    Seek(Duration),
    Next,
    Previous,
    Stop,
    Volume(f64),
    Mute(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("volume must be a finite value between -144 and 0 dB")]
    InvalidVolume,
    #[error("no queue entry in that direction")]
    NoEntry,
    #[error("too many pending commands")]
    Busy,
    #[error("player has stopped")]
    Stopped,
    #[error("receiver rejected the command: {0}")]
    Session(SessionError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandState {
    Completed,
    Superseded,
    Failed(CommandError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    State(PlayerState),
    TrackSubmitted(EntryId),
    TrackAudible(EntryId),
    Command { id: CommandId, state: CommandState },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    pub db: f64,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerStatus {
    pub state: PlayerState,
    pub position: Position,
    pub volume: Volume,
    pub profile: Option<ProfileSelection>,
    pub levels: [f32; 2],
    pub dropped_events: u64,
}

pub struct Control {
    cancel: Cancellation,
    interrupt: AtomicBool,
    volume: Mutex<Option<(CommandId, f64)>>,
}

impl Control {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
    pub fn take_volume(&self) -> Option<f64> {
        self.volume.lock().unwrap().take().map(|(_, db)| db)
    }
}

pub trait PlayerSink {
    fn stream(&mut self, feed: &BufferedFeed, control: &Control) -> Result<BufferedProgress, SessionError>;
    fn flush(&mut self) -> Result<(), SessionError>;
    fn set_volume(&mut self, db: f64) -> Result<(), SessionError>;
    fn receiver_buffer(&self) -> Duration {
        Duration::ZERO
    }
    fn startup_volume(&self) -> f64 {
        STARTUP_VOLUME_DB
    }
    fn profile(&self) -> Option<ProfileSelection> {
        None
    }
}

impl PlayerSink for Session {
    fn stream(&mut self, feed: &BufferedFeed, control: &Control) -> Result<BufferedProgress, SessionError> {
        self.stream_buffered_controlled(feed, &|| control.interrupted(), &mut || control.take_volume())
    }
    fn flush(&mut self) -> Result<(), SessionError> {
        self.flush_buffered()
    }
    fn set_volume(&mut self, db: f64) -> Result<(), SessionError> {
        Session::set_volume(self, db)
    }
    fn receiver_buffer(&self) -> Duration {
        crate::client::buffered_send_lead()
    }
    fn startup_volume(&self) -> f64 {
        self.startup_volume_db()
    }
    fn profile(&self) -> Option<ProfileSelection> {
        Session::profile(self)
    }
}

#[derive(Clone, Copy)]
struct Start {
    entry: EntryId,
    frame: u64,
    offset: u64,
}

struct Shared {
    status: PlayerStatus,
    events: VecDeque<PlayerEvent>,
    starts: VecDeque<Start>,
    levels: VecDeque<(u64, [f32; 2])>,
    lead_frames: u64,
    popped: usize,
    submitted: usize,
    audible: usize,
    last: [Option<EntryId>; 2],
    stop: Option<CommandId>,
}

impl Shared {
    fn emit(&mut self, event: PlayerEvent) {
        if self.events.len() == EVENT_CAPACITY {
            self.events.pop_front();
            self.status.dropped_events += 1;
        }
        self.events.push_back(event);
    }

    fn reply(&mut self, id: CommandId, state: CommandState) {
        self.emit(PlayerEvent::Command { id, state });
    }

    fn set_state(&mut self, state: PlayerState) {
        if self.status.state != state {
            self.status.state = state.clone();
            self.emit(PlayerEvent::State(state));
        }
    }

    fn begin_epoch(&mut self) -> u64 {
        self.starts.clear();
        self.levels.clear();
        (self.popped, self.submitted, self.audible) = (0, 0, 0);
        self.status.position.epoch += 1;
        self.set_state(PlayerState::Buffering);
        self.status.position.epoch
    }

    fn hold(&mut self, entry: EntryId, offset: Duration) {
        let frames = (offset.as_secs_f64() * OUTPUT_RATE as f64).round() as u64;
        let epoch = self.status.position.epoch;
        self.status.position = Position { entry: Some(entry), epoch, submitted_frames: frames, audible_frames: frames, ..Position::default() };
        self.status.levels = [0.0; 2];
    }

    fn entry_at(&self, frame: u64) -> Option<Start> {
        self.starts.iter().rev().find(|s| s.frame <= frame).copied()
    }

    fn announce(&mut self, kind: usize, id: EntryId) {
        if self.last[kind] != Some(id) {
            self.last[kind] = Some(id);
            self.emit(if kind == 0 { PlayerEvent::TrackSubmitted(id) } else { PlayerEvent::TrackAudible(id) });
        }
    }

    fn observe(&mut self, progress: BufferedProgress) {
        let submitted = progress.frames;
        let audible = submitted.saturating_sub(self.lead_frames);
        let current = self.entry_at(submitted);
        let heard = self.entry_at(audible).filter(|_| submitted > self.lead_frames);
        while let Some(start) = self.starts.get(self.submitted - self.popped).filter(|s| s.frame <= submitted).copied() {
            self.submitted += 1;
            self.announce(0, start.entry);
        }
        while let Some(start) = self.starts.get(self.audible - self.popped).filter(|s| heard.is_some() && s.frame <= audible).copied() {
            self.audible += 1;
            self.announce(1, start.entry);
        }
        if heard.is_some() && self.status.state == PlayerState::Buffering {
            self.set_state(PlayerState::Playing);
        }
        while self.popped + 1 < self.audible && self.starts.len() > 1 && self.starts[1].frame <= audible {
            self.popped += 1;
            self.starts.pop_front();
        }
        while self.levels.len() > 1 && self.levels[1].0 <= audible {
            self.levels.pop_front();
        }
        if let Some(&(_, levels)) = self.levels.front().filter(|(frame, _)| heard.is_some() && *frame <= audible) {
            self.status.levels = levels;
        }
        let Some(at) = heard.or(current) else { return };
        self.status.position = Position {
            entry: Some(at.entry),
            epoch: self.status.position.epoch,
            submitted_frames: at.offset + submitted.saturating_sub(at.frame),
            audible_frames: at.offset + if heard.is_some() { audible - at.frame } else { 0 },
            receiver_buffered_frames: submitted - audible,
            local_queued_frames: progress.queued_frames,
            peak_local_queued_frames: progress.peak_queued_frames,
        };
    }
}

pub struct Player {
    shared: Arc<Mutex<Shared>>,
    cancel: Cancellation,
    stopped: Arc<Mutex<bool>>,
    commands: SyncSender<(CommandId, Command)>,
    next_id: AtomicU64,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    pub fn play<S, C>(queue: Queue, connect: C) -> Self
    where
        S: PlayerSink,
        C: FnOnce(&Cancellation) -> Result<S, SessionError> + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared {
            status: PlayerStatus {
                state: PlayerState::Connecting,
                position: Position::default(),
                volume: Volume { db: STARTUP_VOLUME_DB, muted: false },
                profile: None,
                levels: [0.0; 2],
                dropped_events: 0,
            },
            events: VecDeque::from([PlayerEvent::State(PlayerState::Connecting)]),
            starts: VecDeque::new(),
            levels: VecDeque::new(),
            lead_frames: 0,
            popped: 0,
            submitted: 0,
            audible: 0,
            last: [None; 2],
            stop: None,
        }));
        let cancel = Cancellation::default();
        let stopped = Arc::new(Mutex::new(false));
        let (commands, inbox) = sync_channel(COMMAND_CAPACITY);
        let (s, c, st) = (shared.clone(), cancel.clone(), stopped.clone());
        let worker = std::thread::spawn(move || {
            let mut inbox = inbox;
            run(queue, connect, &s, &c, &st, &mut inbox)
        });
        Self { shared, cancel, stopped, commands, next_id: AtomicU64::new(1), worker: Some(worker) }
    }

    pub fn send(&self, command: Command) -> Result<CommandId, CommandError> {
        if matches!(command, Command::Volume(db) if !(MIN_VOLUME_DB..=0.0).contains(&db)) {
            return Err(CommandError::InvalidVolume);
        }
        if self.is_finished() {
            return Err(CommandError::Stopped);
        }
        let id = CommandId(self.next_id.fetch_add(1, Ordering::Relaxed));
        if command == Command::Stop {
            let mut s = self.shared.lock().unwrap();
            if let Some(previous) = s.stop.replace(id) {
                s.reply(previous, CommandState::Superseded);
            }
            drop(s);
            self.stop();
            return Ok(id);
        }
        match self.commands.try_send((id, command)) {
            Ok(()) => Ok(id),
            Err(TrySendError::Full(_)) => Err(CommandError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(CommandError::Stopped),
        }
    }

    pub fn stop(&self) {
        *self.stopped.lock().unwrap() = true;
        self.cancel.cancel();
    }

    pub fn status(&self) -> PlayerStatus {
        self.shared.lock().unwrap().status.clone()
    }

    pub fn events(&self) -> Vec<PlayerEvent> {
        self.shared.lock().unwrap().events.drain(..).collect()
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.is_finished())
    }

    pub fn wait(mut self) -> PlayerStatus {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.status()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop();
            let _ = worker.join();
        }
    }
}

type Inbox = Receiver<(CommandId, Command)>;

struct Cursor {
    at: EntryId,
    offset: Duration,
    held: bool,
}

fn neighbor(queue: &Queue, at: EntryId, command: Command) -> Option<EntryId> {
    let mut queue = queue.clone();
    queue.select(at).ok()?;
    if command == Command::Next { queue.peek_next() } else { queue.peek_previous() }
}

fn transport(queue: &Queue, cursor: &mut Cursor, command: Command) -> Result<(), CommandError> {
    match command {
        Command::Pause => cursor.held = true,
        Command::Resume => cursor.held = false,
        Command::Seek(offset) => cursor.offset = queue.get(cursor.at).and_then(|e| e.metadata.duration).map_or(offset, |d| offset.min(d)),
        Command::Next | Command::Previous => {
            cursor.at = neighbor(queue, cursor.at, command).ok_or(CommandError::NoEntry)?;
            cursor.offset = Duration::ZERO;
        }
        Command::Stop | Command::Volume(_) | Command::Mute(_) => {}
    }
    Ok(())
}

fn volume_target(shared: &Mutex<Shared>, command: Command) -> Option<f64> {
    let mut s = shared.lock().unwrap();
    let volume = &mut s.status.volume;
    match command {
        Command::Volume(db) => volume.db = db,
        Command::Mute(muted) => volume.muted = muted,
        _ => return None,
    }
    Some(if volume.muted { MIN_VOLUME_DB } else { volume.db })
}

enum Outcome {
    Ended(Result<(), (EntryId, PlayerError)>),
    Interrupted(CommandId, Command),
}

fn run<S, C>(queue: Queue, connect: C, shared: &Arc<Mutex<Shared>>, cancel: &Cancellation, stopped: &Mutex<bool>, inbox: &mut Inbox)
where
    S: PlayerSink,
    C: FnOnce(&Cancellation) -> Result<S, SessionError> + Send,
{
    let first = queue.peek_next();
    let result = match connect(cancel) {
        Ok(_) if cancel.is_cancelled() => Err(SessionError::Cancelled),
        connected => connected,
    };
    let ended = match (result, first) {
        (Err(error), Some(entry)) => Err((entry, PlayerError::Session(error))),
        (_, None) => Ok(()),
        (Ok(mut sink), Some(at)) => {
            let mut s = shared.lock().unwrap();
            s.lead_frames = (sink.receiver_buffer().as_secs_f64() * OUTPUT_RATE as f64) as u64;
            s.status.profile = sink.profile();
            s.status.volume.db = sink.startup_volume().clamp(MIN_VOLUME_DB, 0.0);
            drop(s);
            drive(&mut sink, &queue, Cursor { at, offset: Duration::ZERO, held: false }, shared, cancel, inbox)
        }
    };
    let mut s = shared.lock().unwrap();
    let state = match ended {
        _ if *stopped.lock().unwrap() => PlayerState::Stopped,
        Err((entry, error)) => PlayerState::Paused { entry, error },
        Ok(()) => PlayerState::Finished,
    };
    s.set_state(state);
    while let Ok((id, _)) = inbox.try_recv() {
        s.reply(id, CommandState::Failed(CommandError::Stopped));
    }
    if let Some(id) = s.stop.take() {
        s.reply(id, CommandState::Completed);
    }
}

fn drive<S: PlayerSink>(sink: &mut S, queue: &Queue, mut cursor: Cursor, shared: &Arc<Mutex<Shared>>, cancel: &Cancellation, inbox: &mut Inbox) -> Result<(), (EntryId, PlayerError)> {
    while !cancel.is_cancelled() {
        if !cursor.held {
            match play_epoch(sink, queue, &cursor, shared, cancel, inbox) {
                Outcome::Ended(result) => return result,
                Outcome::Interrupted(id, command) => {
                    let position = shared.lock().unwrap().status.position;
                    cursor.at = position.entry.unwrap_or(cursor.at);
                    cursor.offset = Duration::from_nanos(position.audible_frames * 1_000_000_000 / OUTPUT_RATE as u64);
                    if let Err(error) = sink.flush() {
                        shared.lock().unwrap().reply(id, CommandState::Failed(CommandError::Session(error.clone())));
                        return Err((cursor.at, PlayerError::Session(error)));
                    }
                    let result = transport(queue, &mut cursor, command);
                    let mut s = shared.lock().unwrap();
                    s.hold(cursor.at, cursor.offset);
                    if cursor.held {
                        s.set_state(PlayerState::Held { entry: cursor.at });
                    }
                    s.reply(id, result.map_or_else(CommandState::Failed, |()| CommandState::Completed));
                }
            }
            continue;
        }
        let Ok((id, command)) = inbox.recv_timeout(POLL) else { continue };
        let result = match volume_target(shared, command) {
            Some(db) => sink.set_volume(db).map_err(CommandError::Session),
            None => transport(queue, &mut cursor, command),
        };
        let mut s = shared.lock().unwrap();
        if !matches!(command, Command::Volume(_) | Command::Mute(_)) {
            s.hold(cursor.at, cursor.offset);
            if cursor.held {
                s.set_state(PlayerState::Held { entry: cursor.at });
            }
        }
        s.reply(id, result.map_or_else(CommandState::Failed, |()| CommandState::Completed));
    }
    Ok(())
}

fn play_epoch<S: PlayerSink>(sink: &mut S, queue: &Queue, cursor: &Cursor, shared: &Arc<Mutex<Shared>>, cancel: &Cancellation, inbox: &mut Inbox) -> Outcome {
    let epoch = shared.lock().unwrap().begin_epoch();
    let (producer, feed) = buffered_queue(OUTPUT_RATE);
    let control = Control { cancel: cancel.clone(), interrupt: AtomicBool::new(false), volume: Mutex::new(None) };
    let decoder = {
        let (queue, at, offset, shared, cancel) = (queue.clone(), cursor.at, cursor.offset, shared.clone(), cancel.clone());
        std::thread::spawn(move || decode_queue(queue, at, offset, producer, &shared, &cancel, epoch))
    };
    let done = AtomicBool::new(false);
    let (streamed, (pending, interrupt)) = std::thread::scope(|scope| {
        let polling = scope.spawn(|| poll(queue, cursor, shared, &feed, &control, &done, inbox));
        let streamed = sink.stream(&feed, &control);
        done.store(true, Ordering::SeqCst);
        (streamed, polling.join().unwrap_or_default())
    });
    let progress = feed.progress();
    drop(feed);
    let decoded = decoder.join().unwrap_or(Ok(()));
    let mut s = shared.lock().unwrap();
    let applied = control.volume.lock().unwrap().take();
    if let Some((id, db)) = applied {
        drop(s);
        let result = sink.set_volume(db);
        s = shared.lock().unwrap();
        s.reply(id, result.map_or_else(|e| CommandState::Failed(CommandError::Session(e)), |()| CommandState::Completed));
    } else if let Some(id) = pending {
        let state = match &streamed {
            Err(error) if interrupt.is_none() => CommandState::Failed(CommandError::Session(error.clone())),
            _ => CommandState::Completed,
        };
        s.reply(id, state);
    }
    if let (Some((id, command)), false) = (interrupt, cancel.is_cancelled()) {
        if streamed.is_ok() || streamed == Err(SessionError::Cancelled) {
            s.observe(progress);
            return Outcome::Interrupted(id, command);
        }
    }
    let lead = std::mem::take(&mut s.lead_frames);
    s.observe(progress);
    s.lead_frames = lead;
    let at = s.status.position.entry.unwrap_or(cursor.at);
    Outcome::Ended(match (streamed, decoded) {
        (Err(error), _) => Err((at, PlayerError::Session(error))),
        (Ok(_), decoded) => decoded,
    })
}

type Pending = (Option<CommandId>, Option<(CommandId, Command)>);

fn poll(queue: &Queue, cursor: &Cursor, shared: &Mutex<Shared>, feed: &BufferedFeed, control: &Control, done: &AtomicBool, inbox: &mut Inbox) -> Pending {
    let (mut pending, mut interrupt) = (None, None);
    while !done.load(Ordering::SeqCst) {
        let mut s = shared.lock().unwrap();
        s.observe(feed.progress());
        if control.volume.lock().unwrap().is_none() {
            if let Some(id) = pending.take() {
                s.reply(id, CommandState::Completed);
            }
        }
        drop(s);
        while let Some((id, command)) = interrupt.is_none().then(|| inbox.try_recv().ok()).flatten() {
            if let Some(db) = volume_target(shared, command) {
                let superseded = control.volume.lock().unwrap().replace((id, db)).is_some();
                if let Some(previous) = pending.replace(id) {
                    shared.lock().unwrap().reply(previous, if superseded { CommandState::Superseded } else { CommandState::Completed });
                }
                continue;
            }
            let mut s = shared.lock().unwrap();
            let at = s.status.position.entry.unwrap_or(cursor.at);
            match command {
                Command::Resume => s.reply(id, CommandState::Completed),
                Command::Next | Command::Previous if neighbor(queue, at, command).is_none() => s.reply(id, CommandState::Failed(CommandError::NoEntry)),
                _ => {
                    if command == Command::Pause {
                        s.set_state(PlayerState::Pausing { entry: at });
                    }
                    interrupt = Some((id, command));
                    control.interrupt.store(true, Ordering::SeqCst);
                }
            }
        }
        std::thread::sleep(POLL);
    }
    (pending, interrupt)
}

fn decode_queue(
    mut queue: Queue,
    at: EntryId,
    mut offset: Duration,
    producer: BufferedProducer,
    shared: &Mutex<Shared>,
    cancel: &Cancellation,
    epoch: u64,
) -> Result<(), (EntryId, PlayerError)> {
    let mut frames = 0u64;
    let mut next = Some(at);
    while let Some(entry) = next.and_then(|id| queue.select(id).ok().cloned()) {
        next = queue.peek_next();
        let mut mark = |offset: u64, pushed: u64, samples: &[f32]| {
            let mut s = shared.lock().unwrap();
            if s.status.position.epoch != epoch {
                return;
            }
            if pushed == 0 {
                s.starts.push_back(Start { entry: entry.id, frame: frames, offset });
            }
            if s.levels.len() == LEVEL_CAPACITY {
                s.levels.pop_front();
            }
            let peak = samples.as_chunks::<2>().0.iter().fold([0f32; 2], |p, f| [p[0].max(f[0].abs()), p[1].max(f[1].abs())]);
            s.levels.push_back((frames + pushed, peak));
        };
        match decode_track(&entry.path, std::mem::take(&mut offset), &producer, cancel, &mut mark) {
            Ok(n) => frames += n,
            Err(Some(message)) => return Err((entry.id, PlayerError::Decode { path: entry.path, message })),
            Err(None) => return Ok(()),
        }
    }
    Ok(())
}

fn decode_track(path: &std::path::Path, seek: Duration, producer: &BufferedProducer, cancel: &Cancellation, mark: &mut dyn FnMut(u64, u64, &[f32])) -> Result<u64, Option<String>> {
    let fail = |e: anyhow::Error| Some(format!("{e:#}"));
    let mut reader = AudioReader::open(path).map_err(fail)?;
    let (rate, channels) = (reader.info().sample_rate, reader.info().channels);
    let mut offset = 0;
    if !seek.is_zero() {
        let frame = (seek.as_secs_f64() * rate as f64).round() as u64;
        if reader.info().total_frames.is_some_and(|total| frame >= total) {
            return Ok(0);
        }
        let start = reader.seek(frame).map_err(fail)?.start_frame;
        offset = (start as u128 * OUTPUT_RATE as u128 / rate as u128) as u64;
    }
    let output = OutputFormat { sample_rate: OUTPUT_RATE, bits: 24 };
    let mut converter: Option<Converter> = None;
    let mut pushed = 0u64;
    let mut push = |samples: &[f32]| {
        if !samples.is_empty() {
            mark(offset, pushed, samples);
        }
        producer.push(samples).map(|_| pushed += (samples.len() / 2) as u64).map_err(|_| None)
    };
    while let Some(block) = reader.next_block().map_err(fail)? {
        if cancel.is_cancelled() {
            return Err(None);
        }
        let conv = match &mut converter {
            Some(conv) => conv,
            None => {
                let source = match block.samples {
                    Samples::Int { bits, .. } => SourceFormat::int(rate, channels, bits.next_multiple_of(8).clamp(16, 32), bits),
                    Samples::Float(_) => SourceFormat::float(rate, channels),
                };
                converter.insert(Converter::new(source, output, 1.0, 0).map_err(fail)?)
            }
        };
        push(conv.push(&block.samples).map_err(fail)?)?;
    }
    if let Some(conv) = &mut converter {
        push(conv.finish().map_err(fail)?)?;
    }
    Ok(pushed)
}

pub fn connect_device(device: Device, devices: Vec<Device>, pin: String, options: SessionOptions) -> impl FnOnce(&Cancellation) -> Result<Session, SessionError> + Send + 'static {
    move |cancel: &Cancellation| {
        let options = SessionOptions { cancellation: cancel.clone(), ..options };
        match StereoPair::resolve(&devices, &device) {
            Some(pair) => Session::connect_pair(&pair, &pin, options),
            None => Session::connect(&device, &pin, options),
        }
    }
}

pub fn discover_device(name: String, timeout: Duration, pin: String, options: SessionOptions) -> impl FnOnce(&Cancellation) -> Result<Session, SessionError> + Send + 'static {
    move |cancel: &Cancellation| {
        let devices = discover_until(timeout, cancel).map_err(|_| SessionError::ReceiverUnavailable)?;
        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        let device = find_device(&devices, &name).cloned().ok_or(SessionError::ReceiverUnavailable)?;
        connect_device(device, devices, pin, options)(cancel)
    }
}
