//! `airplay` — a discover-and-play CLI over airplay-core.
//!
//!   airplay discover
//!   airplay pair  --device NAME [--pin 1234]
//!   airplay play  --device NAME --file song.flac [--mode auto|realtime|buffered]
//!                 (auto plays files as strict buffered PCM 24-bit/48 kHz)
//!                 [--lossless]
//!                 [--format aac44100|aac48000|pcm24-48|pcm24-48be|alac48000-24]
//!                 [--pin 1234] [--volume-db -30] [--no-stereo] [--debug]
//!
//! A stereo pair (two receivers sharing a `tsid`) is detected automatically and
//! driven in lockstep unless `--no-stereo` is given.

use airplay_core::audio::{self, PcmEncoder};
use airplay_core::client::{
    play_buffered_frames_synchronized, play_pcm_synchronized, AudioCodec, BufferedAudioFormat, Client,
};
use airplay_core::discovery::{self, Device};
use airplay_core::profile::{preflight, FallbackPolicy, OutputProfile, QualityReport, SourceFormat};
use airplay_core::SessionProfile;
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("discover");
    let rest = &args[args.len().min(2)..];

    let result = match cmd {
        "discover" => run_discover(),
        "pair" => run_pair(rest),
        "play" => run_play(rest),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown command {other:?}\n");
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "Usage:\n  airplay discover\n  airplay pair --device NAME [--pin 1234]\n  airplay play --device NAME --file FILE [--mode auto|realtime|buffered] [--lossless]\n               [--group] [--format aac44100|aac48000|pcm24-48|pcm24-48be|alac48000-24]\n               [--pin 1234] [--volume-db -30] [--no-stereo] [--debug]\n               [--oracle HOST:PORT] [--fairplay-helper PATH] [--fairplay-audio-helper PATH]\n\n  --group  experimental: stream into an entertainment/home-theater group by\n           targeting the Apple TV group leader (see docs/ENTERTAINMENT_GROUPS.md)\n  --oracle HOST:PORT  use a Python MITM oracle helper (bin/fairplay_oracle.py)\n                      to capture the sender-side FairPlay response\n  --fairplay-helper PATH  use a Doubletake fp-m3 binary to compute setup2\n                          from the Apple TV's fresh M2 reply\n  --fairplay-audio-helper PATH  path to the Doubletake fp-audio-key helper\n                                (defaults to fp-audio-key next to --fairplay-helper)"
    );
}

// ---- argument parsing ------------------------------------------------------

struct Args {
    flags: std::collections::HashMap<String, String>,
    bools: std::collections::HashSet<String>,
}

impl Args {
    fn parse(input: &[String], bool_flags: &[&str]) -> Self {
        let mut flags = std::collections::HashMap::new();
        let mut bools = std::collections::HashSet::new();
        let mut i = 0;
        while i < input.len() {
            let a = &input[i];
            if let Some(name) = a.strip_prefix("--") {
                if bool_flags.contains(&name) {
                    bools.insert(name.to_string());
                    i += 1;
                } else if let Some(eq) = name.find('=') {
                    flags.insert(name[..eq].to_string(), name[eq + 1..].to_string());
                    i += 1;
                } else if i + 1 < input.len() {
                    flags.insert(name.to_string(), input[i + 1].clone());
                    i += 2;
                } else {
                    bools.insert(name.to_string());
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        Self { flags, bools }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(|s| s.as_str())
    }
    fn has(&self, key: &str) -> bool {
        self.bools.contains(key)
    }
}

// ---- logging ---------------------------------------------------------------

struct StderrLogger;
impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}
static LOGGER: StderrLogger = StderrLogger;

fn enable_debug() {
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug));
    eprintln!("[debug] logging enabled");
}

// ---- commands --------------------------------------------------------------

fn run_discover() -> Result<()> {
    println!("Discovering AirPlay devices...");
    let devices = discovery::load_or_discover("", Duration::from_secs(5), Duration::ZERO)?;
    if devices.is_empty() {
        println!("no AirPlay devices found");
        return Ok(());
    }
    for d in &devices {
        let ip = d.ipv4.map(|v| v.to_string()).unwrap_or_default();
        let ap2 = if d.supports_ap2() { "✓" } else { "✗" };
        let buf = if d.supports_buffered_audio() { " buffered" } else { "" };
        let stereo = if !d.record.tight_sync_id.is_empty() {
            format!(" [pair:{:.8}]", d.record.tight_sync_id)
        } else {
            String::new()
        };
        println!(
            "{:<30} {:<18} {:<15} AP2:{}{}{}",
            d.name, d.record.model, ip, ap2, buf, stereo
        );
    }
    Ok(())
}

fn run_pair(input: &[String]) -> Result<()> {
    let args = Args::parse(input, &["debug"]);
    if args.has("debug") {
        enable_debug();
    }
    let name = args.get("device").ok_or_else(|| anyhow!("required: --device NAME"))?;
    let pin = args.get("pin").unwrap_or("3939");

    let (_devices, dev) = lookup(name)?;
    if dev.is_homepod() {
        println!("{} is a HomePod — it pairs transiently at play time, no pairing needed.", dev.name);
        return Ok(());
    }
    println!("Pairing with {} (PIN {})...", dev.name, pin);
    Client::pair(&dev, pin)?;
    println!("Paired with {}. Identity saved.", dev.name);
    Ok(())
}

fn run_play(input: &[String]) -> Result<()> {
    let args = Args::parse(input, &["debug", "no-stereo", "lossless", "group"]);
    if args.has("debug") {
        enable_debug();
    }

    let name = args.get("device").ok_or_else(|| anyhow!("required: --device NAME"))?;
    let file = args.get("file").map(|s| s.to_string());
    let pin = args.get("pin").unwrap_or("3939").to_string();
    let volume_db: f64 = args.get("volume-db").unwrap_or("-30").parse().context("invalid --volume-db")?;
    let mode = args.get("mode").unwrap_or("auto");
    let lossless = args.has("lossless");
    // --lossless is shorthand for buffered ALAC unless an explicit format is set.
    let format_name = args.get("format").unwrap_or(if lossless { "alac48000-24" } else { "pcm24-48" });

    println!("Looking up {name:?}...");
    let (devices, dev) = lookup(name)?;
    println!("Found: {} ({})", dev.name, dev.record.model);

    // Entertainment-group mode: target this device as the group leader (Apple
    // TV) and let it fan audio out to its members. Implies buffered; skips
    // stereo-pair handling (a group is one logical target).
    let group = args.has("group");
    let oracle = args.get("oracle").or_else(|| {
        if args.has("oracle") {
            Some("127.0.0.1:9999")
        } else {
            None
        }
    });
    let fairplay_helper = args.get("fairplay-helper");
    let fairplay_audio_helper = args.get("fairplay-audio-helper");
    let buffered = if group {
        true
    } else {
        match mode {
            "realtime" => false,
            "buffered" => true,
            "auto" => lossless || file.is_some(),
            other => bail!("--mode must be auto, realtime, or buffered (got {other:?})"),
        }
    };
    let format = parse_buffered_format(format_name)?;
    let requested = if buffered {
        SessionProfile::Buffered(format.clone())
    } else {
        SessionProfile::Realtime { sample_rate: 44100 }
    };
    if buffered {
        preflight(&dev.route().streams, &requested, &FallbackPolicy::Forbid).map_err(|e| {
            anyhow!("{}: {e}; buffered {} is strict, use --mode realtime to opt into realtime", dev.name, format.name)
        })?;
        report_quality(file.as_deref(), &requested)?;
    }

    // Stereo pair? Two receivers sharing a tsid, played in lockstep. (Not for
    // group mode.)
    if !group && !args.has("no-stereo") {
        if let Some(mate) = find_mate(&devices, &dev) {
            println!("🎵 Stereo pair: {} + {}", dev.name, mate.name);
            return run_stereo(&dev, &mate, &pin, file.as_deref(), volume_db, buffered, &format);
        }
        if !dev.record.tight_sync_id.is_empty() {
            println!("   {} has a pair tsid but its mate wasn't found (single-speaker mode)", dev.name);
        }
    }

    run_single(&dev, &pin, file.as_deref(), volume_db, buffered, group, &format, oracle, fairplay_helper, fairplay_audio_helper)
}

fn run_single(
    dev: &Device,
    pin: &str,
    file: Option<&str>,
    volume_db: f64,
    buffered: bool,
    group: bool,
    format: &BufferedAudioFormat,
    oracle: Option<&str>,
    fairplay_helper: Option<&str>,
    fairplay_audio_helper: Option<&str>,
) -> Result<()> {
    let file = match file {
        Some(f) => f,
        None => {
            if buffered {
                bail!("buffered mode requires --file");
            }
            let mut client = connect_and_setup(dev, pin, volume_db, buffered, group, format, None, oracle, fairplay_helper, fairplay_audio_helper, 44100)?;
            println!("Streaming silence. Press Ctrl+C to stop.");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };

    let result = if buffered {
        let mut client = connect_and_setup(dev, pin, volume_db, buffered, group, format, None, oracle, fairplay_helper, fairplay_audio_helper, 0)?;
        let frames = build_buffered_frames(file, format)?;
        println!("Playing {file} buffered {} ({} frames)...", format.name, frames.len());
        let r = play_buffered_frames_synchronized(std::slice::from_mut(&mut client), frames);
        client.stop().ok();
        r?;
        Ok(())
    } else {
        let (samples, sr, ch) = audio::load_audio(file, 48000, 2).context("decode")?;
        let mut client = connect_and_setup(dev, pin, volume_db, buffered, group, format, None, oracle, fairplay_helper, fairplay_audio_helper, sr)?;
        println!("Playing {file} realtime ({sr}Hz/{ch}ch)...");
        let r = play_pcm_synchronized(std::slice::from_mut(&mut client), &samples, sr, ch);
        client.stop().ok();
        r
    };

    result?;
    println!("Done.");
    Ok(())
}

fn run_stereo(
    left: &Device,
    right: &Device,
    pin: &str,
    file: Option<&str>,
    volume_db: f64,
    buffered: bool,
    format: &BufferedAudioFormat,
) -> Result<()> {
    let file = match file {
        Some(f) => f,
        None => bail!("stereo playback requires --file"),
    };

    // Decode/encode once, then fan out to both speakers.
    let (frames, samples, sr) = if buffered {
        (Some(build_buffered_frames(file, format)?), None, 0)
    } else {
        let (s, sample_rate, _ch) = audio::load_audio(file, 48000, 2).context("decode")?;
        (None, Some(s), sample_rate)
    };

    // The left speaker starts the PTP clock + session UUID; the right joins it.
    let left_client = connect_and_setup(left, pin, volume_db, buffered, false, format, None, None, None, None, sr)?;
    let right_client = connect_and_setup(right, pin, volume_db, buffered, false, format, Some(&left_client), None, None, None, sr)?;
    let mut clients = vec![left_client, right_client];

    let result = if buffered {
        let frames = frames.unwrap();
        println!("Playing {file} buffered {} on stereo pair ({} frames)...", format.name, frames.len());
        play_buffered_frames_synchronized(&mut clients, frames)
    } else {
        let samples = samples.unwrap();
        println!("Playing {file} realtime on stereo pair ({sr}Hz)...");
        play_pcm_synchronized(&mut clients, &samples, sr, 2)
    };

    for c in &mut clients {
        c.stop().ok();
    }
    result?;
    println!("Done.");
    Ok(())
}

/// Connect, pair, and set up a stream. When `share` is given, this speaker
/// joins that speaker's PTP timing group and session UUID (stereo pair).
fn connect_and_setup(
    dev: &Device,
    pin: &str,
    volume_db: f64,
    buffered: bool,
    group: bool,
    format: &BufferedAudioFormat,
    share: Option<&Client>,
    oracle: Option<&str>,
    fairplay_helper: Option<&str>,
    fairplay_audio_helper: Option<&str>,
    realtime_sample_rate: i32,
) -> Result<Client> {
    let mut client = Client::new();
    client.set_startup_volume(volume_db);
    if let Some(o) = oracle {
        client.set_oracle(o);
    }
    if let Some(h) = fairplay_helper {
        client.set_fairplay_helper(h);
    }
    if let Some(h) = fairplay_audio_helper {
        client.set_fairplay_audio_helper(h);
    }

    if let Some(other) = share {
        if let Some(ptp) = other.ptp_clock() {
            client.set_ptp_clock(ptp);
        }
        let g = other.group_uuid().to_string();
        if !g.is_empty() {
            client.set_group_uuid(&g);
        }
    }

    println!("Connecting to {}...", dev.name);
    client.connect(dev, pin).with_context(|| format!("connect {}", dev.name))?;

    let label = if group {
        format!("entertainment-group {}", format.name)
    } else if buffered {
        format!("buffered {}", format.name)
    } else {
        "realtime".to_string()
    };
    println!("Setting up {label} stream for {}...", dev.name);
    if group {
        client.setup_group(format).with_context(|| format!("group setup {}", dev.name))?;
    } else if buffered {
        client.setup_buffered(format).with_context(|| format!("setup {}", dev.name))?;
    } else {
        client.setup_realtime(realtime_sample_rate, 2).with_context(|| format!("setup {}", dev.name))?;
    }
    println!("Stream ready for {}.", dev.name);
    Ok(client)
}

// ---- helpers ---------------------------------------------------------------

fn lookup(name: &str) -> Result<(Vec<Device>, Device)> {
    let devices = discovery::load_or_discover(name, Duration::from_secs(5), Duration::from_secs(1800))?;
    let dev = discovery::find_device(&devices, name)
        .cloned()
        .ok_or_else(|| anyhow!("device {name:?} not found"))?;
    Ok((devices, dev))
}

/// The other speaker of a stereo pair: same non-empty tsid, different address.
fn find_mate(devices: &[Device], dev: &Device) -> Option<Device> {
    let tsid = &dev.record.tight_sync_id;
    if tsid.is_empty() {
        return None;
    }
    devices
        .iter()
        .find(|d| &d.record.tight_sync_id == tsid && d.address() != dev.address())
        .cloned()
}

fn parse_buffered_format(name: &str) -> Result<BufferedAudioFormat> {
    let n = name.to_ascii_lowercase().replace('_', "-");
    Ok(match n.as_str() {
        "aac" | "aac44100" | "aac-44100" | "aac44" | "aac44k" => BufferedAudioFormat::aac_44100_stereo(),
        "aac48000" | "aac-48000" | "aac48" | "aac48k" => BufferedAudioFormat::aac_48000_stereo(),
        "pcm" | "pcm24" | "pcm24-48" | "pcm48000-24" => BufferedAudioFormat::pcm_48000_24_stereo(true),
        "pcm24-48be" | "pcm48000-24be" => BufferedAudioFormat::pcm_48000_24_stereo(false),
        "alac" | "alac48" | "alac24-48" | "alac48000-24" => BufferedAudioFormat::alac_48000_24_stereo(),
        other => bail!("unknown --format {other:?} (use aac44100, aac48000, pcm24-48, pcm24-48be, alac48000-24)"),
    })
}

/// Decode/encode `file` into the buffered frames for `format`. Shared by the
/// single-speaker and stereo paths so a pair always streams identical bytes.
fn build_buffered_frames(file: &str, format: &BufferedAudioFormat) -> Result<Vec<Vec<u8>>> {
    let spf = format.samples_per_frame as usize;
    match format.codec {
        AudioCodec::Aac => {
            println!("Encoding {file} as AAC...");
            load_aac_file(file, format.sample_rate, format.channels)
        }
        AudioCodec::Pcm => {
            let (samples, sr, ch) = audio::load_audio(file, format.sample_rate, format.channels)?;
            let enc = PcmEncoder::new(sr, ch, format.bits_per_sample, format.little_endian);
            let frame_samples = spf * ch as usize;
            Ok(chunked(&samples, frame_samples)
                .map(|c| enc.encode(c))
                .collect())
        }
        AudioCodec::Alac => {
            if file.ends_with(".alac") {
                return load_alac_frames(file);
            }
            let (samples, _, _) = audio::load_audio(file, format.sample_rate, format.channels)?;
            let frame_samples = spf * 2;
            Ok(chunked(&samples, frame_samples)
                .map(|c| audio::encode_alac_escape_frame(c, spf))
                .collect())
        }
    }
}

fn report_quality(file: Option<&str>, requested: &SessionProfile) -> Result<()> {
    let output = OutputProfile::of(requested).map_err(|e| anyhow!("{e}"))?;
    println!("Output: {output}");
    if let Some(info) = file.and_then(|f| audio::AudioReader::open(f).ok()).map(|r| r.info().clone()) {
        let q = QualityReport::new(SourceFormat::from(&info), output);
        let bits = q.source.bits_per_sample.map(|b| format!("/{b}-bit")).unwrap_or_default();
        let conversion = match (q.lossless_end_to_end, q.resampled || q.remixed || q.requantized) {
            (true, false) => "native",
            (true, true) => "converted losslessly",
            (false, _) if !q.source.lossless => "converted from lossy source",
            (false, _) => "converted",
        };
        println!("Source: {} {} Hz{bits}/{}ch ({conversion})", q.source.codec, q.source.sample_rate, q.source.channels);
    }
    Ok(())
}

fn chunked(samples: &[f32], frame_samples: usize) -> impl Iterator<Item = &[f32]> {
    samples.chunks_exact(frame_samples)
}

fn load_aac_file(path: &str, sample_rate: i32, channels: i32) -> Result<Vec<Vec<u8>>> {
    let data = std::fs::read(path).with_context(|| format!("read {path}"))?;
    if data.len() >= 4 && &data[0..4] == b"RIFF" {
        return audio::encode_aac_file(path, sample_rate, channels);
    }
    match audio::parse_adts_frames(&data) {
        Ok(frames) if !frames.is_empty() => {
            let mut first = 0;
            while first < frames.len() && first < 3 && frames[first].len() < 35 {
                first += 1;
            }
            Ok(frames[first..].to_vec())
        }
        _ => audio::encode_aac_file(path, sample_rate, channels),
    }
}

/// Read a stream of `[u32 LE len][frame]` records (the firmware's exact ALAC
/// bitstream, as emitted by the Go reference's tools/alacenc).
fn load_alac_frames(path: &str) -> Result<Vec<Vec<u8>>> {
    let data = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let mut frames = Vec::new();
    let mut off = 0;
    while off + 4 <= data.len() {
        let n = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        if off + n > data.len() {
            bail!("truncated ALAC frame at offset {off}");
        }
        frames.push(data[off..off + n].to_vec());
        off += n;
    }
    Ok(frames)
}
