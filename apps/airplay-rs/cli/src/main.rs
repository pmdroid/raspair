//! `airplay` — a discover-and-play CLI over airplay-core.
//!
//!   airplay discover [--json]
//!   airplay pair  --device ID|NAME [--json]
//!   airplay paired [--json]
//!   airplay forget --device ID|NAME [--json]
//!   airplay tui   [PATH...] [--device ID|NAME] [--recursive] [--reduced-motion]
//!   airplay play  PATH... --device ID|NAME [--recursive] [--mode buffered]
//!                 (plays files as strict buffered PCM 24-bit/48 kHz)
//!                 [--codec pcm|pcm-be|alac] [--fallback CODEC,...] [--lossless]
//!                 [--pin 1234] [--volume-db -30] [--no-stereo] [--json] [--debug]
//!
//! A stereo pair (two receivers sharing a `tsid`) is detected automatically and
//! driven in lockstep unless `--no-stereo` is given.

use airplay_core::pairing::{CredentialStore, FileStore};
use anyhow::{anyhow, Result};
use std::sync::Arc;

mod auth;
mod play;
mod tui;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("discover");
    let rest = &args[args.len().min(2)..];
    if let Some(dir) = state_dir_arg(rest) {
        std::env::set_var("AIRPLAY_CONFIG_DIR", dir);
    }

    let result = match cmd {
        "discover" | "pair" | "paired" | "forget" => run_auth(cmd, rest),
        "play" => Ok(play::run(rest)),
        "tui" => Ok(tui::run(rest)),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(0)
        }
        other => {
            eprintln!("unknown command {other:?}\n");
            print_usage();
            std::process::exit(1);
        }
    };

    match result {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn state_dir_arg(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--state-dir" {
            return it.next().filter(|d| !d.is_empty()).cloned();
        }
        if let Some(d) = a.strip_prefix("--state-dir=") {
            return (!d.is_empty()).then(|| d.to_string());
        }
    }
    None
}

fn print_usage() {
    eprintln!(
        "Usage:\n  airplay discover [--json]\n  airplay pair --device ID|NAME [--json]\n  airplay paired [--json]\n  airplay forget --device ID|NAME [--json]\n  airplay play PATH... --device ID|NAME [--file FILE] [--recursive] [--mode buffered]\n               [--codec pcm|pcm-be|alac] [--fallback CODEC,...] [--lossless]\n               [--pin 1234] [--volume-db -30] [--no-stereo] [--json] [--debug]\n  airplay tui [PATH...] [--device ID|NAME] [--recursive] [--reduced-motion]\n\n  --state-dir DIR  pairing and device state (default: $AIRPLAY_CONFIG_DIR, then ~/.airplay)\n  pair prompts for PINs and passwords without echo; without a terminal it\n  prints the required challenge and exits with status 2\n  forget removes only the local record; the receiver is not contacted\n  play queues files in argument order and folders in natural order; --recursive\n  includes subfolders. Output is strict buffered 24-bit/48 kHz; --fallback names\n  other lossless codecs that may be used instead and the effective format is printed.\n  --json writes one event object per line. Exit status: 2 pairing required,\n  3 unsupported profile, 64 invalid invocation, 130 cancelled, 1 playback failure\n  tui opens the terminal player and starts playback when --device is given;\n  NO_COLOR disables color and AIRPLAY_REDUCED_MOTION stops animation"
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

fn run_auth(cmd: &str, rest: &[String]) -> Result<i32> {
    let args = Args::parse(rest, &["debug", "json"]);
    if args.has("debug") {
        enable_debug();
    }
    let store: Arc<dyn CredentialStore> = Arc::new(FileStore::open_default()?);
    let mut backend = auth::Live::new(store.clone());
    let mut prompt = auth::Tty;
    let mut out = std::io::stdout();
    let mut cli = auth::Cli { backend: &mut backend, store: &*store, prompt: &mut prompt, out: &mut out, json: args.has("json") };
    let device = || args.get("device").ok_or_else(|| anyhow!("required: --device ID|NAME"));
    match cmd {
        "discover" => cli.discover(),
        "pair" => cli.pair(device()?),
        "paired" => cli.paired(),
        _ => cli.forget(device()?),
    }
}
