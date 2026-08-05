//! AirPlay 2 session orchestrator — real sender via `airplay-core`.
//!
//! Buffered type-103 stream, **pcm24-48 little-endian** (352-sample frames),
//! live PCM from capture. Client RTSP/TCP is blocking: a dedicated worker
//! thread owns all `Client`s. Fail-soft per speaker (one write failure drops
//! that speaker only).

use crate::control::SpeakerTarget;
use crate::stream_tap;
use airplay_core::client::{begin_live_buffered_synchronized, BufferedAudioFormat, Client};
use airplay_core::discovery::Device;
use airplay_core::protocol::{
    FeatureFlags, TxtRecord, FEATURE_SUPPORTS_AIRPLAY_AUDIO, FEATURE_SUPPORTS_BUFFERED_AUDIO,
    FEATURE_SUPPORTS_PTP,
};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// HomePod default PIN (transient pair-setup).
const HOMEPOD_PIN: &str = "3939";

/// PCM24 stereo @ 48 kHz frame: 352 samples × 2 ch × 3 bytes.
const FRAME_BYTES: usize = 352 * 2 * 3;

/// ~0.7 s of frames at 48 kHz (96 × 7.33 ms).
const PCM_CHANNEL_CAP: usize = 96;

#[derive(Debug, Clone, Serialize)]
pub struct AirplaySessionInfo {
    pub name: String,
    pub ip: String,
    pub active: bool,
    pub note: String,
}

enum WorkerMsg {
    Frame(Vec<u8>),
    Volume(f64),
    Stop,
}

struct WorkerHandle {
    tx: SyncSender<WorkerMsg>,
    join: Option<JoinHandle<()>>,
}

struct SharedState {
    sessions: Vec<AirplaySessionInfo>,
}

pub struct AirplayManager {
    /// True when the worker has ≥1 armed client (PCM is accepted).
    active: Arc<AtomicBool>,
    worker: Mutex<Option<WorkerHandle>>,
    shared: Arc<RwLock<SharedState>>,
    /// Residual packed PCM24@48k awaiting a full 352-frame.
    residual: Mutex<Vec<u8>>,
    wire_rate: u32,
    wire_bits: u16,
    channels: u16,
}

impl AirplayManager {
    pub fn new(wire_rate: u32, wire_bits: u16, channels: u16) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            shared: Arc::new(RwLock::new(SharedState {
                sessions: Vec::new(),
            })),
            residual: Mutex::new(Vec::with_capacity(FRAME_BYTES * 2)),
            wire_rate,
            wire_bits,
            channels: channels.max(1),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Start AirPlay sessions toward the given targets (fail-soft per speaker).
    ///
    /// Returns immediately after spawning the RTSP worker (does not block the
    /// session loop for seconds of pairing). `is_active` / `push_pcm` become
    /// live once at least one speaker finishes setup.
    pub fn start(&self, targets: &[SpeakerTarget], volume_db: f32) -> anyhow::Result<()> {
        if targets.is_empty() {
            anyhow::bail!("no AirPlay targets");
        }
        self.stop();

        let targets: Vec<SpeakerTarget> = targets.to_vec();
        {
            let mut s = self.shared.write();
            s.sessions = targets
                .iter()
                .map(|t| AirplaySessionInfo {
                    name: t.name.clone(),
                    ip: t.ip.clone(),
                    active: false,
                    note: "connecting…".into(),
                })
                .collect();
        }
        self.residual.lock().clear();
        self.active.store(false, Ordering::Relaxed);

        let (tx, rx) = mpsc::sync_channel::<WorkerMsg>(PCM_CHANNEL_CAP);
        let shared = self.shared.clone();
        let active = self.active.clone();

        let join = thread::Builder::new()
            .name("airplay-send".into())
            .spawn(move || {
                worker_main(rx, targets, volume_db as f64, shared, active);
            })
            .map_err(|e| anyhow::anyhow!("spawn airplay worker: {e}"))?;

        *self.worker.lock() = Some(WorkerHandle {
            tx,
            join: Some(join),
        });

        Ok(())
    }

    pub fn stop(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.residual.lock().clear();
        let mut guard = self.worker.lock();
        if let Some(mut w) = guard.take() {
            let _ = w.tx.send(WorkerMsg::Stop);
            if let Some(j) = w.join.take() {
                let _ = j.join();
            }
        }
        for s in self.shared.write().sessions.iter_mut() {
            s.active = false;
            if s.note == "live" || s.note == "connecting…" {
                s.note = "stopped".into();
            }
        }
    }

    pub fn set_volume(&self, volume_db: f32) {
        let db = (volume_db as f64).clamp(-144.0, 0.0);
        if let Some(w) = self.worker.lock().as_ref() {
            let _ = w.tx.try_send(WorkerMsg::Volume(db));
        }
    }

    /// Feed a capture wire block (same format as recorder / stream_tap).
    /// Converts to 48 kHz PCM24 LE and pushes complete 352-sample frames.
    pub fn push_pcm(&self, wire: &[u8]) {
        if !self.is_active() || wire.is_empty() {
            return;
        }
        let packed =
            stream_tap::wire_to_stream_pcm(wire, self.wire_bits, self.wire_rate, self.channels);
        if packed.is_empty() {
            return;
        }

        let mut residual = self.residual.lock();
        residual.extend_from_slice(&packed);

        let tx = {
            let guard = self.worker.lock();
            match guard.as_ref() {
                Some(w) => w.tx.clone(),
                None => return,
            }
        };

        while residual.len() >= FRAME_BYTES {
            let frame: Vec<u8> = residual.drain(..FRAME_BYTES).collect();
            match tx.try_send(WorkerMsg::Frame(frame)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    tracing::trace!("airplay: frame queue full, dropping");
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.active.store(false, Ordering::Relaxed);
                    break;
                }
            }
        }
    }

    pub fn info(&self, targets: &[SpeakerTarget]) -> Vec<AirplaySessionInfo> {
        let live = self.shared.read().sessions.clone();
        if !live.is_empty() {
            return live;
        }
        targets
            .iter()
            .filter(|t| matches!(t.target_type, crate::control::TargetType::Airplay))
            .map(|t| AirplaySessionInfo {
                name: t.name.clone(),
                ip: t.ip.clone(),
                active: false,
                note: "idle".into(),
            })
            .collect()
    }
}

impl Default for AirplayManager {
    fn default() -> Self {
        Self::new(48_000, 24, 2)
    }
}

// ---- worker ----------------------------------------------------------------

struct LiveClient {
    name: String,
    client: Client,
}

fn worker_main(
    rx: mpsc::Receiver<WorkerMsg>,
    targets: Vec<SpeakerTarget>,
    volume_db: f64,
    shared: Arc<RwLock<SharedState>>,
    active: Arc<AtomicBool>,
) {
    let format = BufferedAudioFormat::pcm_48000_24_stereo(true);
    let groups = group_targets(&targets);
    let mut clients: Vec<LiveClient> = Vec::new();

    for group in groups {
        match setup_and_begin_group(&group, volume_db, &format, &shared) {
            Ok(group_live) => clients.extend(group_live),
            Err(e) => {
                tracing::error!(error = %e, n = group.len(), "airplay: group failed");
                for t in &group {
                    let mut s = shared.write();
                    if let Some(row) = s.sessions.iter_mut().find(|r| r.name == t.name) {
                        if !row.active && row.note == "connecting…" {
                            row.note = format!("error: {e:#}");
                        }
                    }
                }
            }
        }
    }

    if clients.is_empty() {
        active.store(false, Ordering::Relaxed);
        tracing::error!("airplay: no live clients");
        while let Ok(msg) = rx.recv() {
            if matches!(msg, WorkerMsg::Stop) {
                break;
            }
        }
        return;
    }

    active.store(true, Ordering::Relaxed);
    tracing::info!(n = clients.len(), "airplay: live sessions ready");

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WorkerMsg::Frame(frame)) => {
                clients.retain_mut(|c| match c.client.write_buffered_frame(&frame) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            name = %c.name,
                            error = %e,
                            "airplay: write failed; dropping speaker"
                        );
                        let _ = c.client.stop();
                        update_session(&shared, &c.name, false, &format!("error: {e:#}"));
                        false
                    }
                });
                if clients.is_empty() {
                    tracing::error!("airplay: all speakers dead");
                    break;
                }
            }
            Ok(WorkerMsg::Volume(db)) => {
                for c in clients.iter_mut() {
                    if let Err(e) = c.client.set_volume(db) {
                        tracing::debug!(name = %c.name, error = %e, "airplay: set_volume");
                    }
                }
            }
            Ok(WorkerMsg::Stop) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    for mut c in clients {
        let _ = c.client.stop();
        update_session(&shared, &c.name, false, "stopped");
    }
    active.store(false, Ordering::Relaxed);
    tracing::info!("airplay: worker stopped");
}

fn update_session(shared: &RwLock<SharedState>, name: &str, active: bool, note: &str) {
    let mut s = shared.write();
    if let Some(row) = s.sessions.iter_mut().find(|r| r.name == name) {
        row.active = active;
        row.note = note.to_string();
    }
}

/// Group targets: same non-empty `tight_sync_id` → stereo pair; others alone.
fn group_targets(targets: &[SpeakerTarget]) -> Vec<Vec<SpeakerTarget>> {
    let mut by_tsid: HashMap<String, Vec<SpeakerTarget>> = HashMap::new();
    let mut singles: Vec<Vec<SpeakerTarget>> = Vec::new();
    for t in targets {
        let tsid = t.tight_sync_id.trim();
        if tsid.is_empty() {
            singles.push(vec![t.clone()]);
        } else {
            by_tsid.entry(tsid.to_string()).or_default().push(t.clone());
        }
    }
    let mut out = singles;
    for (_tsid, group) in by_tsid {
        if group.len() >= 2 {
            tracing::info!(
                n = group.len(),
                names = ?group.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                "airplay: stereo / tight-sync group"
            );
            out.push(group);
        } else {
            for t in group {
                out.push(vec![t]);
            }
        }
    }
    out
}

/// Connect + setup_buffered for each target in the group (share PTP/group UUID
/// within the group), then arm RECORD/anchor for live write.
fn setup_and_begin_group(
    group: &[SpeakerTarget],
    volume_db: f64,
    format: &BufferedAudioFormat,
    shared: &RwLock<SharedState>,
) -> anyhow::Result<Vec<LiveClient>> {
    if group.is_empty() {
        anyhow::bail!("empty group");
    }

    let mut staged: Vec<LiveClient> = Vec::new();
    let mut leader_ptp: Option<std::sync::Arc<airplay_core::PtpClock>> = None;
    let mut leader_uuid = String::new();

    for (i, t) in group.iter().enumerate() {
        update_session(shared, &t.name, false, "connecting…");
        let dev = device_from_target(t);
        let mut client = Client::new();
        client.set_startup_volume(volume_db);

        if i > 0 {
            if let Some(ref ptp) = leader_ptp {
                client.set_ptp_clock(ptp.clone());
            }
            if !leader_uuid.is_empty() {
                client.set_group_uuid(&leader_uuid);
            }
        }

        if let Err(e) = client.connect(&dev, HOMEPOD_PIN) {
            tracing::error!(name = %t.name, ip = %t.ip, error = %e, "airplay: connect failed");
            update_session(shared, &t.name, false, &format!("error: connect {e:#}"));
            continue;
        }

        if let Err(e) = client.setup_buffered(format) {
            tracing::error!(name = %t.name, error = %e, "airplay: setup_buffered failed");
            let _ = client.stop();
            update_session(shared, &t.name, false, &format!("error: setup {e:#}"));
            continue;
        }

        if i == 0 || leader_ptp.is_none() {
            leader_ptp = client.ptp_clock();
            leader_uuid = client.group_uuid().to_string();
        }

        tracing::info!(name = %t.name, ip = %t.ip, "airplay: connected (buffered pcm24-48)");
        staged.push(LiveClient {
            name: t.name.clone(),
            client,
        });
    }

    if staged.is_empty() {
        anyhow::bail!("all speakers in group failed connect/setup");
    }

    // Arm playback: shared PTP anchor for multi-speaker groups.
    if staged.len() == 1 {
        let lc = &mut staged[0];
        lc.client
            .begin_buffered_playback()
            .map_err(|e| anyhow::anyhow!("{}: begin: {e:#}", lc.name))?;
        update_session(shared, &lc.name, true, "live");
        return Ok(staged);
    }

    // Stereo / multi: extract clients, begin synchronized, rebuild LiveClient list.
    let names: Vec<String> = staged.iter().map(|c| c.name.clone()).collect();
    let mut raw: Vec<Client> = staged.into_iter().map(|c| c.client).collect();

    match begin_live_buffered_synchronized(&mut raw) {
        Ok(()) => {
            let mut out = Vec::with_capacity(raw.len());
            for (name, client) in names.into_iter().zip(raw.into_iter()) {
                update_session(shared, &name, true, "live");
                out.push(LiveClient { name, client });
            }
            Ok(out)
        }
        Err(e) => {
            tracing::warn!(error = %e, "airplay: synchronized begin failed; falling back per-speaker");
            let mut out = Vec::new();
            for (name, mut client) in names.into_iter().zip(raw.into_iter()) {
                match client.begin_buffered_playback() {
                    Ok(()) => {
                        update_session(shared, &name, true, "live");
                        out.push(LiveClient { name, client });
                    }
                    Err(e2) => {
                        tracing::error!(name = %name, error = %e2, "airplay: begin failed");
                        let _ = client.stop();
                        update_session(shared, &name, false, &format!("error: begin {e2:#}"));
                    }
                }
            }
            if out.is_empty() {
                Err(anyhow::anyhow!("begin failed for all speakers: {e:#}"))
            } else {
                Ok(out)
            }
        }
    }
}

fn device_from_target(t: &SpeakerTarget) -> Device {
    let ipv4 = t.ip.parse::<Ipv4Addr>().ok();
    let model = t
        .model
        .clone()
        .filter(|m| !m.is_empty())
        // Default to HomePod family so transient pair-setup is used when mDNS
        // did not carry a model (product path is HomePod / HomePod mini).
        .unwrap_or_else(|| "AudioAccessory".into());
    let mut features = FeatureFlags(0);
    features.0 |= 1u64 << FEATURE_SUPPORTS_AIRPLAY_AUDIO;
    features.0 |= 1u64 << FEATURE_SUPPORTS_BUFFERED_AUDIO;
    features.0 |= 1u64 << FEATURE_SUPPORTS_PTP;

    Device {
        name: t.name.clone(),
        host: t.ip.clone(),
        port: if t.port == 0 { 7000 } else { t.port },
        ipv4,
        ipv6: None,
        record: TxtRecord {
            device_id: t.device_id.clone(),
            features,
            model,
            tight_sync_id: t.tight_sync_id.clone(),
            ..Default::default()
        },
        last_seen: 0,
    }
}
