use airplay_core::audio::{scan, AudioReader, Queue, Samples, ScanOptions, Track, TrackMetadata};
use airplay_core::stream::{BufferedCodec, BufferedFeed, BufferedProgress, BufferedSink, BufferedWriter};
use airplay_core::{BufferedAudioFormat, Cancellation, Device, Player, PlayerError, PlayerEvent, PlayerSink, PlayerState, SessionError, SessionOptions};
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
    delay: Duration,
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
        let ints = payload.as_chunks::<3>().0.iter().map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8);
        self.0.samples.lock().unwrap().extend(ints);
        Ok(())
    }
}

impl PlayerSink for Fake {
    fn stream(&mut self, feed: &BufferedFeed, cancel: &Cancellation) -> Result<BufferedProgress, SessionError> {
        while self.hold && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        let writer = BufferedWriter {
            codec: BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(true))?,
            samples_per_frame: 352,
            interval: Duration::from_nanos(352 * 1_000_000_000 / 48_000),
            anchor_local: Instant::now(),
            send_lead: Duration::from_secs(3600),
        };
        let mut sinks = [Collector(self, 0)];
        writer.run(&mut sinks, feed, &|| cancel.is_cancelled(), &mut |_| {}).map_err(|e| e.downcast_ref::<SessionError>().cloned().unwrap_or(SessionError::ConnectionLost))
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
