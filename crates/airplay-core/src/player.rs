use crate::audio::{AudioReader, Converter, EntryId, OutputFormat, Queue, Samples, SourceFormat};
use crate::discovery::{discover_until, find_device};
use crate::stream::{buffered_queue, BufferedFeed, BufferedProducer, BufferedProgress};
use crate::{Cancellation, Device, Session, SessionError, SessionOptions, StereoPair};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const OUTPUT_RATE: u32 = 48_000;
pub const EVENT_CAPACITY: usize = 64;
const POLL: Duration = Duration::from_millis(10);

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
    Finished,
    Stopped,
    Paused { entry: EntryId, error: PlayerError },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub entry: Option<EntryId>,
    pub submitted_frames: u64,
    pub audible_frames: u64,
    pub receiver_buffered_frames: u64,
    pub local_queued_frames: u64,
    pub peak_local_queued_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    State(PlayerState),
    TrackSubmitted(EntryId),
    TrackAudible(EntryId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerStatus {
    pub state: PlayerState,
    pub position: Position,
    pub dropped_events: u64,
}

pub trait PlayerSink {
    fn stream(&mut self, feed: &BufferedFeed, cancel: &Cancellation) -> Result<BufferedProgress, SessionError>;
    fn receiver_buffer(&self) -> Duration {
        Duration::ZERO
    }
}

impl PlayerSink for Session {
    fn stream(&mut self, feed: &BufferedFeed, _cancel: &Cancellation) -> Result<BufferedProgress, SessionError> {
        self.stream_buffered(feed)
    }
    fn receiver_buffer(&self) -> Duration {
        crate::client::buffered_send_lead()
    }
}

struct Shared {
    status: PlayerStatus,
    events: VecDeque<PlayerEvent>,
    starts: VecDeque<(EntryId, u64)>,
    lead_frames: u64,
    popped: usize,
    submitted: usize,
    audible: usize,
}

impl Shared {
    fn emit(&mut self, event: PlayerEvent) {
        if self.events.len() == EVENT_CAPACITY {
            self.events.pop_front();
            self.status.dropped_events += 1;
        }
        self.events.push_back(event);
    }

    fn set_state(&mut self, state: PlayerState) {
        if self.status.state != state {
            self.status.state = state.clone();
            self.emit(PlayerEvent::State(state));
        }
    }

    fn entry_at(&self, frame: u64) -> Option<(EntryId, u64)> {
        self.starts.iter().rev().find(|(_, start)| *start <= frame).copied()
    }

    fn observe(&mut self, progress: BufferedProgress) {
        let submitted = progress.frames;
        let audible = submitted.saturating_sub(self.lead_frames);
        let current = self.entry_at(submitted);
        let heard = self.entry_at(audible).filter(|_| submitted > self.lead_frames);
        while let Some(&(id, _)) = self.starts.get(self.submitted - self.popped).filter(|(_, start)| *start <= submitted) {
            self.submitted += 1;
            self.emit(PlayerEvent::TrackSubmitted(id));
        }
        while let Some(&(id, _)) = self.starts.get(self.audible - self.popped).filter(|(_, start)| heard.is_some() && *start <= audible) {
            self.audible += 1;
            self.emit(PlayerEvent::TrackAudible(id));
        }
        if heard.is_some() && self.status.state == PlayerState::Buffering {
            self.set_state(PlayerState::Playing);
        }
        let (entry, start) = heard.or(current).map_or((None, 0), |(id, start)| (Some(id), start));
        while self.popped + 1 < self.audible && self.starts.len() > 1 && self.starts[1].1 <= audible {
            self.popped += 1;
            self.starts.pop_front();
        }
        self.status.position = Position {
            entry,
            submitted_frames: submitted.saturating_sub(start),
            audible_frames: if heard.is_some() { audible - start } else { 0 },
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
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    pub fn play<S, C>(queue: Queue, connect: C) -> Self
    where
        S: PlayerSink,
        C: FnOnce(&Cancellation) -> Result<S, SessionError> + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared {
            status: PlayerStatus { state: PlayerState::Connecting, position: Position::default(), dropped_events: 0 },
            events: VecDeque::from([PlayerEvent::State(PlayerState::Connecting)]),
            starts: VecDeque::new(),
            lead_frames: 0,
            popped: 0,
            submitted: 0,
            audible: 0,
        }));
        let cancel = Cancellation::default();
        let stopped = Arc::new(Mutex::new(false));
        let (s, c, st) = (shared.clone(), cancel.clone(), stopped.clone());
        let worker = std::thread::spawn(move || run(queue, connect, &s, &c, &st));
        Self { shared, cancel, stopped, worker: Some(worker) }
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

fn run<S, C>(queue: Queue, connect: C, shared: &Arc<Mutex<Shared>>, cancel: &Cancellation, stopped: &Mutex<bool>)
where
    S: PlayerSink,
    C: FnOnce(&Cancellation) -> Result<S, SessionError> + Send,
{
    let first = queue.peek_next();
    let (producer, feed) = buffered_queue(OUTPUT_RATE);
    let decoder = {
        let (shared, cancel) = (shared.clone(), cancel.clone());
        std::thread::spawn(move || decode_queue(queue, producer, &shared, &cancel))
    };
    let streamed = std::thread::scope(|scope| {
        let streaming = scope.spawn(|| {
            let mut sink = connect(cancel)?;
            if cancel.is_cancelled() {
                return Err(SessionError::Cancelled);
            }
            let mut s = shared.lock().unwrap();
            s.lead_frames = (sink.receiver_buffer().as_secs_f64() * OUTPUT_RATE as f64) as u64;
            s.set_state(PlayerState::Buffering);
            drop(s);
            sink.stream(&feed, cancel)
        });
        while !streaming.is_finished() {
            shared.lock().unwrap().observe(feed.progress());
            std::thread::sleep(POLL);
        }
        streaming.join().unwrap_or(Err(SessionError::ConnectionLost))
    });
    let progress = feed.progress();
    drop(feed);
    let decoded = decoder.join().unwrap_or(Ok(()));
    let mut s = shared.lock().unwrap();
    s.lead_frames = 0;
    s.observe(progress);
    let at = s.entry_at(progress.frames).map(|(id, _)| id).or(first);
    let state = match (streamed, decoded) {
        _ if *stopped.lock().unwrap() => PlayerState::Stopped,
        (Err(error), _) => match at {
            Some(entry) => PlayerState::Paused { entry, error: PlayerError::Session(error) },
            None => PlayerState::Stopped,
        },
        (Ok(_), Err((entry, error))) => PlayerState::Paused { entry, error },
        (Ok(_), Ok(())) => PlayerState::Finished,
    };
    s.set_state(state);
}

fn decode_queue(mut queue: Queue, producer: BufferedProducer, shared: &Mutex<Shared>, cancel: &Cancellation) -> Result<(), (EntryId, PlayerError)> {
    let mut frames = 0u64;
    while let Some(entry) = queue.advance().cloned() {
        let mut started = || shared.lock().unwrap().starts.push_back((entry.id, frames));
        match decode_track(&entry.path, &producer, cancel, &mut started) {
            Ok(n) => frames += n,
            Err(Some(message)) => return Err((entry.id, PlayerError::Decode { path: entry.path, message })),
            Err(None) => return Ok(()),
        }
    }
    Ok(())
}

fn decode_track(path: &std::path::Path, producer: &BufferedProducer, cancel: &Cancellation, started: &mut dyn FnMut()) -> Result<u64, Option<String>> {
    let fail = |e: anyhow::Error| Some(format!("{e:#}"));
    let mut reader = AudioReader::open(path).map_err(fail)?;
    let (rate, channels) = (reader.info().sample_rate, reader.info().channels);
    let output = OutputFormat { sample_rate: OUTPUT_RATE, bits: 24 };
    let mut converter: Option<Converter> = None;
    let mut pushed = 0u64;
    let mut push = |samples: &[f32]| {
        if pushed == 0 && !samples.is_empty() {
            started();
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
