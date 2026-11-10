use crate::auth;
use airplay_core::audio::{scan, EntryId, Queue, ScanOptions, SkipReason};
use airplay_core::discovery::{self, Device};
use airplay_core::pairing::{CredentialStore, FileStore};
use airplay_core::player::connect_device;
use airplay_core::profile::preflight;
use airplay_core::protocol::HOMEPOD_PIN;
use airplay_core::{
    BufferedAudioFormat, Cancellation, FallbackPolicy, Player, PlayerError, PlayerEvent, PlayerSink, PlayerState, PlayerStatus, ProfileSelection, SessionError,
    SessionOptions, SessionProfile, StereoPair,
};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const FAILED: i32 = 1;
pub const PAIRING_REQUIRED: i32 = 2;
pub const UNSUPPORTED_PROFILE: i32 = 3;
pub const USAGE: i32 = 64;
pub const CANCELLED: i32 = 130;

const VALUES: [&str; 9] = ["device", "file", "mode", "codec", "format", "fallback", "pin", "volume-db", "state-dir"];
const SWITCHES: [&str; 5] = ["recursive", "json", "lossless", "no-stereo", "debug"];
const TICK: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct Fail {
    pub code: i32,
    pub message: String,
}

fn usage(message: String) -> Fail {
    Fail { code: USAGE, message }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Pcm,
    PcmBe,
    Alac,
}

impl Codec {
    fn parse(name: &str) -> Result<Self, Fail> {
        match name.to_ascii_lowercase().replace('_', "-").as_str() {
            "pcm" | "pcm24" | "pcm24-48" | "pcm48000-24" => Ok(Self::Pcm),
            "pcm-be" | "pcm24-48be" | "pcm48000-24be" => Ok(Self::PcmBe),
            "alac" | "alac48" | "alac24-48" | "alac48000-24" => Ok(Self::Alac),
            n if n.starts_with("aac") => Err(Fail { code: UNSUPPORTED_PROFILE, message: format!("{name:?} is lossy AAC; use --codec pcm, pcm-be or alac") }),
            _ => Err(usage(format!("unknown codec {name:?} (use pcm, pcm-be or alac)"))),
        }
    }

    fn profile(self) -> SessionProfile {
        SessionProfile::Buffered(match self {
            Self::Pcm => BufferedAudioFormat::pcm_48000_24_stereo(true),
            Self::PcmBe => BufferedAudioFormat::pcm_48000_24_stereo(false),
            Self::Alac => BufferedAudioFormat::alac_48000_24_stereo(),
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct PlayArgs {
    pub paths: Vec<PathBuf>,
    pub device: String,
    pub recursive: bool,
    pub codec: Codec,
    pub fallback: Vec<Codec>,
    pub pin: String,
    pub volume_db: f64,
    pub stereo: bool,
    pub json: bool,
    pub debug: bool,
}

impl PlayArgs {
    pub fn parse(input: &[String]) -> Result<Self, Fail> {
        let (mut paths, mut values, mut switches) = (Vec::new(), Vec::new(), Vec::new());
        let mut it = input.iter();
        let mut literal = false;
        while let Some(arg) = it.next() {
            if literal || !arg.starts_with('-') || arg == "-" {
                paths.push(PathBuf::from(arg));
                continue;
            }
            if arg == "--" {
                literal = true;
                continue;
            }
            let flag = arg.strip_prefix("--").ok_or_else(|| usage(format!("unknown flag {arg}")))?;
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (flag, None),
            };
            if SWITCHES.contains(&name) && inline.is_none() {
                switches.push(name);
            } else if VALUES.contains(&name) {
                let value = inline.or_else(|| it.next().cloned()).ok_or_else(|| usage(format!("--{name} needs a value")))?;
                match name {
                    "file" => paths.push(PathBuf::from(value)),
                    _ => values.push((name, value)),
                }
            } else {
                return Err(usage(format!("unknown flag --{name}")));
            }
        }
        let get = |key: &str| values.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        let has = |key: &str| switches.contains(&key);
        let device = get("device").ok_or_else(|| usage("required: --device ID|NAME".into()))?.to_string();
        if paths.is_empty() {
            return Err(usage("required: one or more PATH arguments (files or folders)".into()));
        }
        match get("mode").unwrap_or("buffered") {
            "auto" | "buffered" => {}
            "realtime" => return Err(Fail { code: UNSUPPORTED_PROFILE, message: "the library player streams files as buffered 24-bit/48 kHz only; realtime file playback is not available".into() }),
            other => return Err(usage(format!("--mode must be buffered or realtime (got {other:?})"))),
        }
        let codec = match get("codec").or(get("format")) {
            Some(name) => Codec::parse(name)?,
            None if has("lossless") => Codec::Alac,
            None => Codec::Pcm,
        };
        let mut fallback = Vec::new();
        for list in values.iter().filter(|(k, _)| *k == "fallback") {
            for name in list.1.split(',').filter(|n| !n.is_empty()) {
                let alternative = Codec::parse(name)?;
                if alternative != codec && !fallback.contains(&alternative) {
                    fallback.push(alternative);
                }
            }
        }
        let volume_db = match get("volume-db").unwrap_or("-30").parse::<f64>() {
            Ok(db) if (-144.0..=0.0).contains(&db) => db,
            _ => return Err(usage("--volume-db must be a number between -144 and 0".into())),
        };
        Ok(Self {
            paths,
            device,
            recursive: has("recursive"),
            codec,
            fallback,
            pin: get("pin").unwrap_or(HOMEPOD_PIN).to_string(),
            volume_db,
            stereo: !has("no-stereo"),
            json: has("json"),
            debug: has("debug"),
        })
    }

    pub fn options(&self, credentials: Option<Arc<dyn CredentialStore>>) -> SessionOptions {
        let fallback = if self.fallback.is_empty() { FallbackPolicy::Forbid } else { FallbackPolicy::Allow(self.fallback.iter().map(|c| c.profile()).collect()) };
        SessionOptions { profile: self.codec.profile(), fallback, volume_db: self.volume_db, credentials, ..SessionOptions::default() }
    }
}

pub fn code_of(error: &SessionError) -> i32 {
    match error {
        SessionError::ChallengeRequired | SessionError::PasswordRequired | SessionError::Authentication | SessionError::UnsupportedAuthentication(_) => PAIRING_REQUIRED,
        SessionError::UnsupportedProfile | SessionError::RealtimeBitDepth { .. } | SessionError::RealtimeRate { .. } => UNSUPPORTED_PROFILE,
        SessionError::MemberFailed { error, .. } => code_of(error),
        SessionError::Cancelled => CANCELLED,
        _ => FAILED,
    }
}

pub struct Out<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub json: bool,
    pub tty: bool,
    pub inline: bool,
}

impl Out<'_> {
    fn line(&mut self) {
        if std::mem::take(&mut self.inline) {
            let _ = writeln!(self.out);
        }
    }

    fn emit(&mut self, value: Value, text: Option<String>) {
        if self.json {
            let _ = writeln!(self.out, "{value}");
        } else if let Some(text) = text {
            self.line();
            let _ = writeln!(self.out, "{text}");
        }
    }

    fn fail(&mut self, code: i32, message: &str) -> i32 {
        if self.json {
            let _ = writeln!(self.out, "{}", json!({"event": "error", "code": code, "message": message}));
        } else {
            self.line();
            let _ = writeln!(self.err, "error: {message}");
        }
        code
    }

    fn progress(&mut self, queue: &Queue, status: &PlayerStatus) {
        let Some(entry) = status.position.entry.and_then(|id| queue.get(id)) else { return };
        let position = Duration::from_millis(status.position.audible_frames * 1000 / 48_000);
        let duration = entry.metadata.duration;
        if self.json {
            let _ = writeln!(self.out, "{}", json!({"event": "progress", "path": entry.path, "position_ms": position.as_millis() as u64, "duration_ms": duration.map(|d| d.as_millis() as u64)}));
        } else if self.tty {
            let total = duration.map(|d| format!(" / {}", clock(d))).unwrap_or_default();
            let _ = write!(self.out, "\r  {}{total}   ", clock(position));
            let _ = self.out.flush();
            self.inline = true;
        }
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn state_name(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Connecting => "connecting",
        PlayerState::Buffering => "buffering",
        PlayerState::Playing => "playing",
        PlayerState::Pausing { .. } => "pausing",
        PlayerState::Held { .. } => "paused",
        PlayerState::Finished => "finished",
        PlayerState::Stopped => "stopped",
        PlayerState::Paused { .. } => "failed",
    }
}

fn report_profile(out: &mut Out, selection: &ProfileSelection) {
    let fallback = selection.fallback.map(|reason| format!("{reason:?}"));
    let text = match &fallback {
        None => format!("Format: {}", selection.effective),
        Some(reason) => format!("Format: {} (fallback from {}: {reason})", selection.effective, selection.requested),
    };
    out.emit(json!({"event": "format", "requested": selection.requested.to_string(), "effective": selection.effective.to_string(), "fallback": fallback}), Some(text));
}

fn report_event(out: &mut Out, queue: &Queue, event: PlayerEvent) {
    let track = |id: EntryId| (queue.position(id).map_or(0, |i| i + 1), queue.get(id).map(|e| e.path.clone()).unwrap_or_default());
    match event {
        PlayerEvent::State(PlayerState::Paused { .. }) | PlayerEvent::Command { .. } | PlayerEvent::TrackSubmitted(_) => {}
        PlayerEvent::State(state) => {
            let name = state_name(&state);
            let text = matches!(state, PlayerState::Buffering).then(|| "Buffering...".to_string());
            out.emit(json!({"event": "state", "state": name}), text);
        }
        PlayerEvent::TrackAudible(id) => {
            let (index, path) = track(id);
            let text = format!("Playing [{index}/{}] {}", queue.len(), path.display());
            out.emit(json!({"event": "track", "index": index, "total": queue.len(), "path": path}), Some(text));
        }
    }
}

pub fn play<S, C>(
    args: &PlayArgs,
    devices: &[Device],
    connect: impl FnOnce(Device, Vec<Device>, SessionOptions) -> C,
    options: SessionOptions,
    out: &mut Out,
    interrupted: &dyn Fn() -> bool,
) -> i32
where
    S: PlayerSink,
    C: FnOnce(&Cancellation) -> Result<S, SessionError> + Send + 'static,
{
    let device = match auth::select(devices, &args.device) {
        Ok(device) => device.clone(),
        Err(e) => return out.fail(USAGE, &format!("{e:#}")),
    };
    if let Err(e) = preflight(&device.route(), &options.profile, &options.fallback) {
        return out.fail(code_of(&e), &format!("{}: {e}; pass --fallback with another lossless codec to allow a different format", args.device));
    }
    let scanned = match scan(&args.paths, &ScanOptions { recursive: args.recursive, ..ScanOptions::default() }, &Cancellation::default()) {
        Ok(report) => report,
        Err(e) => return out.fail(USAGE, &e.to_string()),
    };
    for skipped in &scanned.skipped {
        let reason = match &skipped.reason {
            SkipReason::Unreadable(r) | SkipReason::Unsupported(r) => r,
        };
        out.emit(json!({"event": "skipped", "path": skipped.path, "reason": reason}), Some(format!("Skipping {}: {reason}", skipped.path.display())));
    }
    let mut queue = Queue::new();
    if scanned.tracks.is_empty() || queue.append(scanned.tracks).is_err() {
        return out.fail(USAGE, "no playable files in the given paths (pass --recursive to include subfolders)");
    }
    let devices = if args.stereo { devices.to_vec() } else { Vec::new() };
    let id = device.record.device_id.clone();
    let pair = StereoPair::resolve(&devices, &device).map(|p| [p.left.name, p.right.name]);
    let text = match &pair {
        Some([left, right]) => format!("Receiver: {} ({id}), stereo pair {left} + {right}", device.name),
        None => format!("Receiver: {} ({id})", device.name),
    };
    out.emit(json!({"event": "receiver", "id": id, "name": device.name, "stereo": pair}), Some(text));
    out.emit(json!({"event": "queue", "paths": queue.entries().iter().map(|e| &e.path).collect::<Vec<_>>()}), Some(format!("Queue: {} track(s)", queue.len())));

    let player = Player::play(queue.clone(), connect(device, devices, options));
    let (mut reported, mut ticks, mut stopping) = (false, 0u32, false);
    loop {
        if !stopping && interrupted() {
            stopping = true;
            player.stop();
        }
        let finished = player.is_finished();
        let status = player.status();
        if let Some(selection) = status.profile.filter(|_| !reported) {
            reported = true;
            report_profile(out, &selection);
        }
        for event in player.events() {
            report_event(out, &queue, event);
        }
        if finished {
            break;
        }
        ticks += 1;
        if ticks % 10 == 0 && status.state == PlayerState::Playing {
            out.progress(&queue, &status);
        }
        std::thread::sleep(TICK);
    }
    match player.wait().state {
        PlayerState::Finished => (out.emit(json!({"event": "done"}), Some("Done.".into())), 0).1,
        PlayerState::Paused { error: PlayerError::Session(e), .. } => {
            let code = code_of(&e);
            let hint = if code == PAIRING_REQUIRED { format!("; run `airplay pair --device {id}` first") } else { String::new() };
            out.fail(code, &format!("{e}{hint}"))
        }
        PlayerState::Paused { error, .. } => out.fail(FAILED, &error.to_string()),
        _ if stopping => out.fail(CANCELLED, "playback cancelled"),
        state => out.fail(FAILED, &format!("player ended in state {}", state_name(&state))),
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn run(input: &[String]) -> i32 {
    let args = match PlayArgs::parse(input) {
        Ok(args) => args,
        Err(fail) => {
            eprintln!("error: {}", fail.message);
            return fail.code;
        }
    };
    if args.debug {
        crate::enable_debug();
    }
    let (mut stdout, mut stderr) = (std::io::stdout(), std::io::stderr());
    let tty = stdout.is_terminal();
    let mut out = Out { out: &mut stdout, err: &mut stderr, json: args.json, tty, inline: false };
    let found = FileStore::open_default().map_err(anyhow::Error::from).and_then(|store| Ok((store, discovery::load_or_discover("", Duration::from_secs(5), auth::CACHE_AGE)?)));
    let (store, devices) = match found {
        Ok(found) => found,
        Err(e) => return out.fail(FAILED, &format!("{e:#}")),
    };
    let store: Arc<dyn CredentialStore> = Arc::new(store);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(signal, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    let pin = args.pin.clone();
    let options = args.options(Some(store));
    let interrupted = || {
        let hit = INTERRUPTED.load(Ordering::SeqCst);
        if hit {
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
        }
        hit
    };
    play(&args, &devices, |device, devices, options| connect_device(device, devices, pin, options), options, &mut out, &interrupted)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use airplay_core::protocol::{FeatureFlags, TxtRecord};
    use airplay_core::stream::{BufferedCodec, BufferedFeed, BufferedProgress, BufferedSink, BufferedWriter};
    use airplay_core::{Control, OutputProfile};
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Instant;

    const BUFFERED: u64 = 1 << 9 | 1 << 40 | 1 << 46 | 1 << 48;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn device(id: &str, name: &str, features: u64) -> Device {
        Device {
            name: name.into(),
            host: format!("{id}.local"),
            port: 7000,
            ipv4: Some("192.0.2.10".parse().unwrap()),
            ipv6: None,
            record: TxtRecord { device_id: id.into(), model: "AudioAccessory5,1".into(), features: FeatureFlags(features), has_flags: true, ..Default::default() },
            last_seen: 0,
        }
    }

    pub(crate) fn write_wav(path: &Path, frames: u32) {
        let data = vec![0u8; frames as usize * 6];
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

    struct Discard;

    impl BufferedSink for Discard {
        fn send_frame(&mut self, _: &[u8], _: &dyn Fn() -> bool) -> airplay_core::Result<()> { Ok(()) }
    }

    pub(crate) struct Fake {
        pub(crate) profile: Option<ProfileSelection>,
        pub(crate) hold: bool,
    }

    impl PlayerSink for Fake {
        fn stream(&mut self, feed: &BufferedFeed, control: &Control) -> Result<BufferedProgress, SessionError> {
            while self.hold && !control.cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            let writer = BufferedWriter {
                codec: BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(true))?,
                samples_per_frame: 352,
                interval: Duration::ZERO,
                anchor_local: Instant::now(),
                send_lead: Duration::from_secs(3600),
            };
            writer
                .run_with(&mut [Discard], feed, &|| control.cancelled(), &|| control.interrupted(), &mut |_| {}, &mut |_| Ok(()))
                .map_err(|_| SessionError::Cancelled)
        }
        fn flush(&mut self) -> Result<(), SessionError> { Ok(()) }
        fn set_volume(&mut self, _: f64) -> Result<(), SessionError> { Ok(()) }
        fn profile(&self) -> Option<ProfileSelection> { self.profile }
    }

    struct Run {
        code: i32,
        out: String,
        err: String,
        connected: bool,
    }

    fn run_with(input: &str, devices: &[Device], sink: Result<Fake, SessionError>, interrupted: &dyn Fn() -> bool) -> Run {
        let args = match PlayArgs::parse(&args(input)) {
            Ok(args) => args,
            Err(fail) => return Run { code: fail.code, out: String::new(), err: fail.message, connected: false },
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let connected = Arc::new(Mutex::new(false));
        let flag = connected.clone();
        let mut o = Out { out: &mut out, err: &mut err, json: args.json, tty: false, inline: false };
        let options = args.options(None);
        let code = play(&args, devices, move |_, _, _| move |_: &Cancellation| {
            *flag.lock().unwrap() = true;
            sink
        }, options, &mut o, interrupted);
        let connected = *connected.lock().unwrap();
        Run { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap(), connected }
    }

    fn run(input: &str, devices: &[Device], sink: Result<Fake, SessionError>) -> Run {
        run_with(input, devices, sink, &|| false)
    }

    fn fake() -> Result<Fake, SessionError> {
        Ok(Fake { profile: None, hold: false })
    }

    #[test]
    fn folders_and_files_play_in_argument_then_natural_order() {
        let dir = tempfile::tempdir().unwrap();
        let album = dir.path().join("album");
        std::fs::create_dir_all(album.join("disc")).unwrap();
        for name in ["10.wav", "2.wav", "1.wav", "disc/1.wav"] {
            write_wav(&album.join(name), 4800);
        }
        write_wav(&dir.path().join("single.wav"), 4800);
        let input = format!("{} --device aa:01 --file {} --recursive", dir.path().join("single.wav").display(), album.display());
        let r = run(&input, &[device("aa:01", "Kitchen", BUFFERED)], fake());
        assert_eq!(r.code, 0, "{}{}", r.out, r.err);
        let played: Vec<&str> = r.out.lines().filter_map(|l| l.strip_prefix("Playing ")).collect();
        let expected: Vec<String> = ["single.wav", "album/1.wav", "album/2.wav", "album/10.wav", "album/disc/1.wav"]
            .iter()
            .enumerate()
            .map(|(i, p)| format!("[{}/5] {}", i + 1, dir.path().join(p).display()))
            .collect();
        assert_eq!(played, expected);
        assert!(r.out.ends_with("Done.\n"));
    }

    #[test]
    fn invocation_errors_fail_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.wav");
        write_wav(&file, 480);
        let f = file.display();
        std::fs::create_dir_all(dir.path().join("nested/sub")).unwrap();
        write_wav(&dir.path().join("nested/sub/b.wav"), 480);
        let devices = [device("aa:01", "Kitchen", BUFFERED), device("aa:02", "kitchen", BUFFERED), device("aa:03", "Radio", 1 << 9)];
        for (input, code, message) in [
            (format!("{f} --device aa:01 --group"), USAGE, "unknown flag --group"),
            (format!("{f} --device aa:01 -x"), USAGE, "unknown flag -x"),
            (format!("{f} --device aa:01 --codec flac"), USAGE, "unknown codec"),
            (format!("{f} --device aa:01 --format aac44100"), UNSUPPORTED_PROFILE, "lossy AAC"),
            (format!("{f} --device aa:01 --mode realtime"), UNSUPPORTED_PROFILE, "realtime"),
            (format!("{f} --device aa:01 --volume-db 5"), USAGE, "--volume-db"),
            ("--device aa:01".to_string(), USAGE, "PATH"),
            (format!("{f}"), USAGE, "--device"),
            (format!("{f} --device nowhere"), USAGE, "not found"),
            (format!("{f} --device Kitchen"), USAGE, "matches 2 receivers"),
            (format!("{} --device aa:01", dir.path().join("nested").display()), USAGE, "--recursive"),
            (format!("{f} --device aa:03"), UNSUPPORTED_PROFILE, "unsupported audio profile"),
            (format!("{f} --device aa:03 --fallback alac"), UNSUPPORTED_PROFILE, "unsupported audio profile"),
        ] {
            let r = run(&input, &devices, fake());
            assert_eq!((r.code, r.connected), (code, false), "{input}: {}", r.err);
            assert!(r.err.contains(message), "{input}: {}", r.err);
        }
    }

    #[test]
    fn strict_by_default_and_allowed_fallback_prints_its_effective_format() {
        let strict = PlayArgs::parse(&args("a.wav --device x")).unwrap().options(None);
        assert!(matches!(strict.fallback, FallbackPolicy::Forbid) && OutputProfile::of(&strict.profile).unwrap().is_buffered_lossless_24_48());
        let alac = PlayArgs::parse(&args("a.wav --device x --lossless --fallback pcm,alac")).unwrap();
        assert_eq!((alac.codec, alac.fallback.as_slice()), (Codec::Alac, &[Codec::Pcm][..]));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.wav");
        write_wav(&file, 480);
        let pcm = OutputProfile::of(&Codec::Pcm.profile()).unwrap();
        let alac = OutputProfile::of(&Codec::Alac.profile()).unwrap();
        let selection = ProfileSelection { requested: pcm, effective: alac, fallback: Some(airplay_core::FallbackReason::SetupRejected { status: 400 }) };
        let input = format!("{} --device aa:01 --fallback alac", file.display());
        let r = run(&input, &[device("aa:01", "Kitchen", BUFFERED)], Ok(Fake { profile: Some(selection), hold: false }));
        assert_eq!(r.code, 0);
        assert!(r.out.contains(&format!("Format: {alac} (fallback from {pcm}: SetupRejected {{ status: 400 }})")), "{}", r.out);
        let r = run(&format!("{input} --json"), &[device("aa:01", "Kitchen", BUFFERED)], Ok(Fake { profile: Some(selection), hold: false }));
        let events: Vec<Value> = r.out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let format = events.iter().find(|e| e["event"] == "format").unwrap();
        assert_eq!(format["effective"], alac.to_string());
        assert_eq!(format["requested"], pcm.to_string());
        assert!(!r.out.contains('\x1b') && !r.out.contains('\r'));
        assert_eq!(events.last().unwrap()["event"], "done");
    }

    #[test]
    fn session_errors_map_to_distinct_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.wav");
        write_wav(&file, 480);
        let input = format!("{} --device aa:01", file.display());
        let devices = [device("aa:01", "Kitchen", BUFFERED)];
        for (error, code) in [
            (SessionError::ChallengeRequired, PAIRING_REQUIRED),
            (SessionError::MemberFailed { member: "aa:02".into(), error: Box::new(SessionError::PasswordRequired) }, PAIRING_REQUIRED),
            (SessionError::UnsupportedProfile, UNSUPPORTED_PROFILE),
            (SessionError::ConnectionLost, FAILED),
        ] {
            let r = run(&input, &devices, Err(error.clone()));
            assert_eq!((r.code, r.connected, r.err.contains("airplay pair --device aa:01")), (code, true, code == PAIRING_REQUIRED), "{error:?}");
        }
    }

    #[test]
    fn interrupt_stops_the_player_and_joins_its_workers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.wav");
        write_wav(&file, 48_000);
        let start = Instant::now();
        let r = run_with(&format!("{} --device aa:01 --json", file.display()), &[device("aa:01", "Kitchen", BUFFERED)], Ok(Fake { profile: None, hold: true }), &|| {
            start.elapsed() > Duration::from_millis(300)
        });
        assert_eq!(r.code, CANCELLED, "{}", r.out);
        assert!(r.out.lines().any(|l| l.contains("\"stopped\"")));
        assert!(r.out.lines().all(|l| serde_json::from_str::<Value>(l).is_ok()));
    }
}
