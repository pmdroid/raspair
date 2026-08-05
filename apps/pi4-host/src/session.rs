//! Session manager: signal gate + play/stop intent → AirPlay / Chromecast.
//! Optional auto-record follows the same energy gate (begin/end of music).
//! Live capture PCM is also pushed into active AirPlay sessions.

use crate::airplay::AirplayManager;
use crate::capture::CaptureHandle;
use crate::chromecast::ChromecastManager;
use crate::control::{ControlState, SessionIntent, TargetType};
use crate::music_id;
use crate::record::Recorder;
use crate::settings::SettingsStore;
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
    pub settings: Arc<SettingsStore>,
}

pub async fn run(rt: SessionRuntime) {
    let mut above_since: Option<Instant> = None;
    let mut below_since: Option<Instant> = None;
    let mut rx = rt.capture.subscribe();

    // Drain PCM for recording + gate even when no HTTP clients are connected.
    loop {
        let snap = rt.settings.snapshot();

        // Handle explicit play/stop from HTTP.
        match rt.control.intent() {
            SessionIntent::WantPlay => {
                rt.control.clear_intent();
                start_sessions(&rt, &snap);
            }
            SessionIntent::WantStop => {
                rt.control.clear_intent();
                stop_sessions(&rt, &snap, /*from_gate=*/ false);
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
                    // Live AirPlay: same capture path as recorder (wire PCM →
                    // manager converts to pcm24-48 frames).
                    if rt.airplay.is_active() {
                        rt.airplay.push_pcm(&block.samples);
                    }
                    apply_gate(
                        &rt,
                        &snap,
                        block.rms_dbfs,
                        &mut above_since,
                        &mut below_since,
                    );
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

        if snap.force_open && !rt.control.streaming() {
            start_sessions(&rt, &snap);
        }

        sleep(Duration::from_millis(5)).await;
    }
}

fn apply_gate(
    rt: &SessionRuntime,
    snap: &crate::settings::RuntimeSettings,
    rms: f64,
    above_since: &mut Option<Instant>,
    below_since: &mut Option<Instant>,
) {
    if snap.force_open {
        return;
    }
    let start_th = snap.audio_start_threshold_db;
    let stop_th = snap.audio_stop_threshold_db;
    let hold = Duration::from_millis(snap.audio_start_hold_ms);
    let stop_to = Duration::from_secs(snap.audio_stop_timeout_sec);

    if rms >= start_th {
        below_since.take();
        let since = above_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= hold {
            if !rt.control.streaming() {
                start_sessions(rt, snap);
            }
            // Auto-record: begin when energy holds (even if no speakers selected).
            if snap.auto_record && !rt.recorder.is_recording() {
                match rt.recorder.start(None) {
                    Ok(_) => tracing::info!("auto-record started (energy gate)"),
                    Err(e) => tracing::warn!(error = %e, "auto-record start failed"),
                }
            }
        }
    } else if rms < stop_th {
        above_since.take();
        let quiet_long = below_since
            .get_or_insert_with(Instant::now)
            .elapsed()
            >= stop_to;
        if quiet_long {
            if rt.control.streaming() {
                stop_sessions(rt, snap, /*from_gate=*/ true);
            } else if snap.auto_record && rt.recorder.is_recording() {
                // Auto-record end without an active stream session.
                stop_auto_record(rt, snap);
            }
        }
    } else {
        // Hysteresis band
        above_since.take();
        below_since.take();
    }
}

fn start_sessions(rt: &SessionRuntime, snap: &crate::settings::RuntimeSettings) {
    let targets = rt.control.targets();
    if targets.is_empty() {
        tracing::warn!("play requested but no targets selected");
        // Still allow auto-record without speakers.
        if snap.auto_record && !rt.recorder.is_recording() {
            let _ = rt.recorder.start(None);
        }
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

    if snap.auto_record && !rt.recorder.is_recording() {
        match rt.recorder.start(None) {
            Ok(_) => tracing::info!("auto-record started with session"),
            Err(e) => tracing::warn!(error = %e, "auto-record start failed"),
        }
    }
}

fn stop_sessions(
    rt: &SessionRuntime,
    snap: &crate::settings::RuntimeSettings,
    from_gate: bool,
) {
    rt.airplay.stop();
    rt.chromecast.stop();
    if rt.control.streaming() {
        rt.control.set_streaming(false);
        rt.stats.session_stopped();
        tracing::info!(from_gate, "sessions stopped");
    }
    if snap.auto_record && rt.recorder.is_recording() {
        stop_auto_record(rt, snap);
    }
}

fn stop_auto_record(rt: &SessionRuntime, snap: &crate::settings::RuntimeSettings) {
    match rt.recorder.stop() {
        Ok(_) => {
            tracing::info!("auto-record stopped");
            maybe_identify(rt, snap);
        }
        Err(e) => tracing::warn!(error = %e, "auto-record stop failed"),
    }
}

fn maybe_identify(rt: &SessionRuntime, snap: &crate::settings::RuntimeSettings) {
    if let Some(name) = rt.recorder.take_last_finished() {
        if snap.music_id_enabled {
            music_id::spawn_identify(rt.recorder.record_dir().to_path_buf(), name, snap.clone());
        } else {
            let _ = crate::collection::mark_skipped(
                rt.recorder.record_dir(),
                &name,
                "music_id_enabled is false",
            );
        }
    }
}
