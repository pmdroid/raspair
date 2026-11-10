use airplay_core::audio::{scan, AudioReader, Queue, Samples, ScanOptions, Track, TrackMetadata};
use airplay_core::stream::{BufferedCodec, BufferedFeed, BufferedProgress, BufferedSink, BufferedWriter};
use airplay_core::{BufferedAudioFormat, Cancellation, Command, CommandError, CommandId, CommandState, Control, Device, Player, PlayerError, PlayerEvent, PlayerSink, PlayerState, SessionError, SessionOptions, Volume};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FLAC: &str = "pcm24_48k_stereo.flac";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn write_wav(path: &Path, frames: u32, sample: impl Fn(u32, u32) -> i32) {
    let data: Vec<u8> = (0..frames).flat_map(|n| [0, 1].map(|c| sample(n, c))).flat_map(|v| v.to_le_bytes()[..3].to_vec()).collect();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 0x0002_0001, 48_000, 48_000 * 6, 0x0018_0006] {
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, wav).unwrap();
}

fn source_ints(path: &Path) -> Vec<i32> {
    let mut reader = AudioReader::open(path).unwrap();
    let mut out = Vec::new();
    while let Some(block) = reader.next_block().unwrap() {
        match block.samples {
            Samples::Int { bits: 24, data } => out.extend(data),
            other => panic!("unexpected {other:?}"),
        }
    }
    out
}

fn queue(paths: &[PathBuf]) -> Queue {
    let mut q = Queue::new();
    let meta = TrackMetadata { title: String::new(), artist: None, album: None, track: None, duration: None };
    q.append(paths.iter().map(|p| Track { path: p.clone(), metadata: meta.clone() }).collect()).unwrap();
    q
}

#[derive(Clone, Default)]
struct Fake {
    samples: Arc<Mutex<Vec<i32>>>,
    segments: Arc<Mutex<Vec<Vec<i32>>>>,
    flushes: Arc<Mutex<u32>>,
    volumes: Arc<Mutex<Vec<f64>>>,
    delay: Duration,
    lead: Duration,
    fail_after: Option<u64>,
    hold: bool,
}

struct Collector<'a>(&'a Fake, u64);

impl BufferedSink for Collector<'_> {
    fn send_frame(&mut self, payload: &[u8], cancelled: &dyn Fn() -> bool) -> airplay_core::Result<()> {
        if cancelled() {
            return Err(SessionError::Cancelled.into());
        }
        if self.0.fail_after == Some(self.1) {
            return Err(SessionError::ConnectionLost.into());
        }
        self.1 += 1;
        std::thread::sleep(self.0.delay);
        let ints: Vec<i32> = payload.as_chunks::<3>().0.iter().map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8).collect();
        self.0.segments.lock().unwrap().last_mut().unwrap().extend(&ints);
        self.0.samples.lock().unwrap().extend(ints);
        Ok(())
    }
}

impl PlayerSink for Fake {
    fn stream(&mut self, feed: &BufferedFeed, control: &Control) -> Result<BufferedProgress, SessionError> {
        self.segments.lock().unwrap().push(Vec::new());
        while self.hold && !control.cancelled() && !control.interrupted() {
            std::thread::sleep(Duration::from_millis(2));
        }
        let writer = BufferedWriter {
            codec: BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(true))?,
            samples_per_frame: 352,
            interval: Duration::from_nanos(352 * 1_000_000_000 / 48_000),
            anchor_local: Instant::now(),
            send_lead: Duration::from_secs(3600),
        };
        let volumes = self.volumes.clone();
        let mut sinks = [Collector(self, 0)];
        let mut apply = |_: &mut [Collector]| {
            volumes.lock().unwrap().extend(control.take_volume());
            Ok(())
        };
        writer
            .run_with(&mut sinks, feed, &|| control.cancelled(), &|| control.interrupted(), &mut |_| {}, &mut apply)
            .map_err(|e| e.downcast_ref::<SessionError>().cloned().unwrap_or(SessionError::ConnectionLost))
    }
    fn flush(&mut self) -> Result<(), SessionError> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
    fn set_volume(&mut self, db: f64) -> Result<(), SessionError> {
        self.volumes.lock().unwrap().push(db);
        Ok(())
    }
    fn receiver_buffer(&self) -> Duration {
        self.lead
    }
}

fn play(q: Queue, fake: &Fake) -> Player {
    let fake = fake.clone();
    Player::play(q, move |_: &Cancellation| Ok(fake))
}

fn wait_for(player: &Player, f: impl Fn(&airplay_core::PlayerStatus) -> bool) {
    let start = Instant::now();
    while !f(&player.status()) {
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", player.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn two_file_queue_plays_in_order_with_exact_frame_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let second = dir.path().join("second.wav");
    write_wav(&second, 1001, |n, c| (n as i32 * 7919 + c as i32 * 104_729) % 8_388_607 - 4_000_000);
    let q = queue(&[fixture(FLAC), second.clone()]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let fake = Fake::default();
    let player = play(q, &fake);
    let status = player.wait();
    assert_eq!(status.state, PlayerState::Finished);
    let mut expected = source_ints(&fixture(FLAC));
    assert_eq!(expected.len(), 4800 * 2);
    expected.extend(source_ints(&second));
    let got = fake.samples.lock().unwrap().clone();
    assert_eq!(got.len(), (4800 + 1001_usize).div_ceil(352) * 352 * 2);
    assert_eq!(&got[..expected.len()], &expected[..]);
    assert!(got[expected.len()..].iter().all(|&v| v == 0));
    assert_eq!(status.position.entry, Some(ids[1]));
    assert_eq!(status.position.submitted_frames, 1001);
}

#[test]
fn slow_receiver_and_idle_consumer_stay_bounded() {
    let q = queue(&vec![fixture(FLAC); 100]);
    let fake = Fake { delay: Duration::from_micros(300), ..Default::default() };
    let player = play(q, &fake);
    let mut peak = 0;
    while !player.is_finished() {
        let p = player.status().position;
        assert!(p.local_queued_frames <= 96_000);
        peak = peak.max(p.local_queued_frames);
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = player.wait();
    assert_eq!(status.state, PlayerState::Finished);
    assert!(peak > 0 && status.position.peak_local_queued_frames <= 96_000);
    assert!(status.dropped_events > 0);
    assert_eq!(fake.samples.lock().unwrap().len(), (4800 * 100_usize).div_ceil(352) * 352 * 2);
}

#[test]
fn stop_works_while_pairing_buffering_and_playing() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let device = Device {
        name: "silent".into(),
        host: "localhost".into(),
        port: listener.local_addr().unwrap().port(),
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: airplay_core::protocol::TxtRecord {
            model: "AudioAccessory1,1".into(),
            features: airplay_core::protocol::FeatureFlags(1 << 9 | 1 << 40),
            ..Default::default()
        },
        last_seen: 0,
    };
    let q = queue(&vec![fixture(FLAC); 200]);
    let discovering = Player::play(q.clone(), airplay_core::player::discover_device("absent".into(), Duration::from_secs(30), "1234".into(), SessionOptions::default()));
    let pairing = Player::play(q.clone(), airplay_core::player::connect_device(device, vec![], "1234".into(), SessionOptions::default()));
    let _socket = listener.accept().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(pairing.status().state, PlayerState::Connecting);
    assert_eq!(discovering.status().state, PlayerState::Connecting);
    let held = play(q.clone(), &Fake { hold: true, ..Default::default() });
    wait_for(&held, |s| s.state == PlayerState::Buffering);
    let playing = play(q, &Fake { delay: Duration::from_millis(7), ..Default::default() });
    wait_for(&playing, |s| s.state == PlayerState::Playing && s.position.submitted_frames > 0);
    for player in [discovering, pairing, held, playing] {
        let start = Instant::now();
        player.stop();
        assert_eq!(player.wait().state, PlayerState::Stopped);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

#[test]
fn failures_pause_at_the_affected_entry() {
    let dir = tempfile::tempdir().unwrap();
    let corrupt = dir.path().join("corrupt.flac");
    std::fs::write(&corrupt, b"fLaC not really").unwrap();
    let q = queue(&[fixture(FLAC), corrupt.clone(), fixture(FLAC)]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let fake = Fake::default();
    let status = play(q, &fake).wait();
    assert!(matches!(&status.state, PlayerState::Paused { entry, error: PlayerError::Decode { path, .. } } if *entry == ids[1] && *path == corrupt));
    assert_eq!(fake.samples.lock().unwrap().len(), 4800_usize.div_ceil(352) * 352 * 2);

    let q = queue(&vec![fixture(FLAC); 3]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let player = play(q, &Fake { fail_after: Some(20), ..Default::default() });
    let state = player.wait().state;
    assert_eq!(state, PlayerState::Paused { entry: ids[1], error: PlayerError::Session(SessionError::ConnectionLost) });
}

#[test]
fn library_code_plays_a_scanned_folder() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["Track 10.flac", "Track 2.flac", "Track 1.flac"] {
        std::fs::copy(fixture(FLAC), dir.path().join(name)).unwrap();
    }
    let report = scan(&[dir.path().to_path_buf()], &ScanOptions::default(), &Cancellation::default()).unwrap();
    let mut q = Queue::new();
    let ids = q.append(report.tracks).unwrap();
    let fake = Fake::default();
    let player = play(q, &fake);
    wait_for(&player, |s| s.state == PlayerState::Finished);
    let submitted: Vec<_> = player.events().into_iter().filter_map(|e| match e {
        PlayerEvent::TrackSubmitted(id) => Some(id),
        _ => None,
    }).collect();
    assert_eq!(submitted, ids);
    assert_eq!(fake.samples.lock().unwrap().len(), (3 * 4800_usize).div_ceil(352) * 352 * 2);
}

#[test]
fn track_events_follow_queue_order() {
    let q = queue(&vec![fixture(FLAC); 3]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let player = play(q, &Fake { delay: Duration::from_millis(1), ..Default::default() });
    wait_for(&player, |s| s.state == PlayerState::Finished);
    let tracks: Vec<_> = player.events().into_iter().filter_map(|e| match e {
        PlayerEvent::TrackAudible(id) => Some(id),
        _ => None,
    }).collect();
    assert_eq!(tracks, ids);
}

fn ramp(dir: &Path, name: &str, frames: u32, base: i32) -> PathBuf {
    let path = dir.join(name);
    write_wav(&path, frames, |n, c| if c == 0 { base + n as i32 } else { -(base + n as i32) });
    path
}

fn replies(player: &Player) -> Vec<(airplay_core::CommandId, CommandState)> {
    player.events().into_iter().filter_map(|e| match e {
        PlayerEvent::Command { id, state } => Some((id, state)),
        _ => None,
    }).collect()
}

fn left(segment: &[i32]) -> Vec<i32> {
    segment.iter().step_by(2).copied().collect()
}

#[test]
fn seek_starts_a_new_epoch_without_pre_seek_samples() {
    let dir = tempfile::tempdir().unwrap();
    let q = queue(&[ramp(dir.path(), "a.wav", 48_000, 0), ramp(dir.path(), "b.wav", 1001, 1_000_000)]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let fake = Fake { delay: Duration::from_millis(1), ..Default::default() };
    let player = play(q, &fake);
    wait_for(&player, |s| s.state == PlayerState::Playing && s.position.submitted_frames > 4800);
    let seek = player.send(Command::Seek(Duration::from_millis(500))).unwrap();
    wait_for(&player, |s| s.state == PlayerState::Finished);
    let status = player.status();
    assert_eq!((status.position.entry, status.position.epoch), (Some(ids[1]), 2));
    assert!(replies(&player).contains(&(seek, CommandState::Completed)));
    let segments = fake.segments.lock().unwrap().clone();
    assert_eq!(segments.len(), 2);
    assert_eq!(*fake.flushes.lock().unwrap(), 1);
    let before = left(&segments[0]);
    assert!(before.len() >= 4800 && before.len() < 24_000 && before.iter().enumerate().all(|(i, &v)| v == i as i32));
    let after = left(&segments[1]);
    let expected: Vec<i32> = (24_000..48_000).chain(1_000_000..1_001_001).collect();
    assert_eq!(&after[..expected.len()], &expected[..]);
    assert!(after[expected.len()..].iter().all(|&v| v == 0));
}

fn await_reply(player: &Player, id: CommandId) -> CommandState {
    let start = Instant::now();
    loop {
        if let Some((_, state)) = replies(player).into_iter().find(|(i, _)| *i == id) {
            return state;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", player.status());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn command(player: &Player, command: Command) -> CommandState {
    await_reply(player, player.send(command).unwrap())
}

#[test]
fn pause_resume_and_edge_seeks_keep_queue_selection_and_frames() {
    let dir = tempfile::tempdir().unwrap();
    let q = queue(&[ramp(dir.path(), "a.wav", 48_000, 0), ramp(dir.path(), "b.wav", 48_000, 1_000_000)]);
    let ids: Vec<_> = q.entries().iter().map(|e| e.id).collect();
    let fake = Fake { delay: Duration::from_millis(2), lead: Duration::from_millis(50), ..Default::default() };
    let player = play(q, &fake);
    wait_for(&player, |s| s.state == PlayerState::Playing && s.position.audible_frames > 4800);
    assert_eq!(command(&player, Command::Pause), CommandState::Completed);
    let status = player.status();
    assert_eq!(status.state, PlayerState::Held { entry: ids[0] });
    assert_eq!((status.position.entry, status.position.receiver_buffered_frames), (Some(ids[0]), 0));
    assert_eq!(*fake.flushes.lock().unwrap(), 1);
    let held = status.position.audible_frames;
    assert_eq!(left(&fake.segments.lock().unwrap()[0]).len() as u64, held + 2400);
    assert_eq!(command(&player, Command::Previous), CommandState::Failed(CommandError::NoEntry));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fake.segments.lock().unwrap().len(), 1);
    assert_eq!(command(&player, Command::Resume), CommandState::Completed);
    wait_for(&player, |s| s.position.epoch == 2 && s.state == PlayerState::Playing);
    assert_eq!(command(&player, Command::Seek(Duration::from_secs(10))), CommandState::Completed);
    wait_for(&player, |s| s.position.epoch == 3 && s.state == PlayerState::Playing);
    assert_eq!(player.status().position.entry, Some(ids[1]));
    assert_eq!(command(&player, Command::Previous), CommandState::Completed);
    wait_for(&player, |s| s.position.epoch == 4 && s.state == PlayerState::Playing);
    assert_eq!(command(&player, Command::Seek(Duration::from_millis(1))), CommandState::Completed);
    wait_for(&player, |s| s.state == PlayerState::Finished);
    assert_eq!((player.status().position.entry, player.status().position.epoch), (Some(ids[1]), 5));
    let starts: Vec<i32> = fake.segments.lock().unwrap().iter().map(|seg| seg[0]).collect();
    assert_eq!(starts, [0, held as i32, 1_000_000, 0, 48]);
    let last = left(&fake.segments.lock().unwrap()[4]);
    let expected: Vec<i32> = (48..48_000).chain(1_000_000..1_048_000).collect();
    assert_eq!(&last[..expected.len()], &expected[..]);
}

#[test]
fn rapid_transport_commands_settle_without_deadlock() {
    let q = queue(&vec![fixture(FLAC); 4]);
    let player = play(q, &Fake { delay: Duration::from_millis(1), lead: Duration::from_millis(20), ..Default::default() });
    wait_for(&player, |s| s.state == PlayerState::Playing);
    let start = Instant::now();
    let cycle = [Command::Seek(Duration::from_millis(30)), Command::Next, Command::Seek(Duration::ZERO), Command::Previous, Command::Pause, Command::Resume, Command::Volume(-20.0)];
    let mut accepted = Vec::new();
    for c in cycle.iter().cycle().take(40) {
        match player.send(*c) {
            Ok(id) => accepted.push(id),
            Err(e) => assert_eq!(e, CommandError::Busy),
        }
    }
    let last = player.send(Command::Next).or_else(|_| player.send(Command::Pause));
    accepted.extend(last);
    let stop = player.send(Command::Stop).unwrap();
    wait_for(&player, |s| s.state == PlayerState::Stopped);
    assert!(start.elapsed() < Duration::from_secs(5));
    let mut done: Vec<_> = replies(&player).into_iter().map(|(id, _)| id).collect();
    done.sort();
    accepted.push(stop);
    assert!(accepted.iter().all(|id| done.binary_search(id).is_ok()), "{accepted:?} {done:?}");
    assert_eq!(player.send(Command::Pause), Err(CommandError::Stopped));
}

#[test]
fn volume_is_bounded_and_mute_is_reversible() {
    let q = queue(&vec![fixture(FLAC); 50]);
    let fake = Fake { delay: Duration::from_millis(1), ..Default::default() };
    let player = play(q, &fake);
    for db in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.5, -144.5] {
        assert_eq!(player.send(Command::Volume(db)), Err(CommandError::InvalidVolume));
    }
    assert_eq!(player.status().volume, Volume { db: -30.0, muted: false });
    wait_for(&player, |s| s.state == PlayerState::Playing);
    assert_eq!(command(&player, Command::Mute(true)), CommandState::Completed);
    assert_eq!(command(&player, Command::Volume(-10.0)), CommandState::Completed);
    assert_eq!(player.status().volume, Volume { db: -10.0, muted: true });
    assert_eq!(command(&player, Command::Pause), CommandState::Completed);
    assert_eq!(command(&player, Command::Mute(false)), CommandState::Completed);
    assert_eq!(command(&player, Command::Volume(-144.0)), CommandState::Completed);
    assert_eq!(command(&player, Command::Volume(0.0)), CommandState::Completed);
    assert_eq!(*fake.volumes.lock().unwrap(), [-144.0, -144.0, -10.0, -144.0, 0.0]);
    assert_eq!(player.status().volume, Volume { db: 0.0, muted: false });
}
