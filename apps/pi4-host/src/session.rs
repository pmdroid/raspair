//! Session manager: signal gate + play/stop intent → AirPlay / Chromecast stubs.

use crate::airplay::AirplayManager;
use crate::capture::CaptureHandle;
use crate::chromecast::ChromecastManager;
use crate::config::Config;
use crate::control::{ControlState, SessionIntent, TargetType};
use crate::record::Recorder;
use crate::stats::Stats;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

pub struct SessionRuntime {
    pub control: Arc<ControlState>,
    pub capture: CaptureHandle,
    pub airplay: Arc<AirplayManager>,
    pub chromecast: Arc<ChromecastManager>,
    pub recorder: Arc<Recorder>,
    pub stats: Arc<Stats>,
    pub cfg: Config,
}

pub async fn run(rt: SessionRuntime) {
    let mut above_since: Option<Instant> = None;
    let mut below_since: Option<Instant> = None;
    let mut rx = rt.capture.subscribe();

    // Drain PCM for recording + gate even when no HTTP clients are connected.
    loop {
        // Handle explicit play/stop from HTTP.
        match rt.control.intent() {
            SessionIntent::WantPlay => {
                rt.control.clear_intent();
                start_sessions(&rt);
            }
            SessionIntent::WantStop => {
                rt.control.clear_intent();
                stop_sessions(&rt);
            }
            SessionIntent::Idle => {}
        }

        // Drain available PCM blocks (non-blocking batch).
        let mut got = 0;
        loop {
            match rx.try_recv() {
                Ok(block) => {
                    got += 1;
                    rt.stats.tick_capture();
                    rt.recorder.write_pcm(&block.samples);
                    apply_gate(&rt, block.rms_dbfs, &mut above_since, &mut below_since);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::debug!(lagged = n, "capture consumer lagged");
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    tracing::error!("capture channel closed");
                    return;
                }
            }
            if got > 32 {
                break;
            }
        }

        if rt.cfg.force_open && !rt.control.streaming() {
            start_sessions(&rt);
        }

        sleep(Duration::from_millis(5)).await;
    }
}

fn apply_gate(
    rt: &SessionRuntime,
    rms: f64,
    above_since: &mut Option<Instant>,
    below_since: &mut Option<Instant>,
) {
    if rt.cfg.force_open {
        return;
    }
    let start_th = rt.cfg.audio_start_threshold_db;
    let stop_th = rt.cfg.audio_stop_threshold_db;
    let hold = Duration::from_millis(rt.cfg.audio_start_hold_ms);
    let stop_to = Duration::from_secs(rt.cfg.audio_stop_timeout_sec);

    if rms >= start_th {
        below_since.take();
        let since = above_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= hold && !rt.control.streaming() {
            start_sessions(rt);
        }
    } else if rms < stop_th {
        above_since.take();
        if rt.control.streaming() {
            let since = below_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= stop_to {
                stop_sessions(rt);
            }
        }
    } else {
        // Hysteresis band
        above_since.take();
        below_since.take();
    }
}

fn start_sessions(rt: &SessionRuntime) {
    let targets = rt.control.targets();
    if targets.is_empty() {
        tracing::warn!("play requested but no targets selected");
        return;
    }
    let volume = rt.control.volume_db();
    let ap: Vec<_> = targets
        .iter()
        .filter(|t| t.target_type == TargetType::Airplay)
        .cloned()
        .collect();
    let cc: Vec<_> = targets
        .iter()
        .filter(|t| t.target_type == TargetType::Chromecast)
        .cloned()
        .collect();

    if !ap.is_empty() {
        if let Err(e) = rt.airplay.start(&ap, volume) {
            tracing::error!(error = %e, "airplay start failed");
            return;
        }
    }
    if !cc.is_empty() {
        let url = "http://127.0.0.1/stream.wav"; // placeholder PCM URL
        if let Err(e) = rt.chromecast.start(&cc, url, volume) {
            tracing::error!(error = %e, "chromecast start failed");
            return;
        }
    }
    rt.control.set_streaming(true);
    rt.stats.session_started();
    tracing::info!(n = targets.len(), "sessions started");
}

fn stop_sessions(rt: &SessionRuntime) {
    rt.airplay.stop();
    rt.chromecast.stop();
    if rt.control.streaming() {
        rt.control.set_streaming(false);
        rt.stats.session_stopped();
        tracing::info!("sessions stopped");
    }
}
