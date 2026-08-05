//! AirPlay 2 session orchestrator — real sender via `airplay-core`.
//!
//! Modes (`airplay_mode` in config):
//! - **realtime** (default): type-96 NTP, **PCM16 BE** @ 48 kHz — faster start
//! - **buffered**: type-103 PTP, **PCM24 LE** @ 48 kHz — higher quality
//!
//! Live PCM from capture. Client RTSP/TCP is blocking: a dedicated worker
//! thread owns all `Client`s. Fail-soft per speaker.

use crate::control::SpeakerTarget;
use crate::stream_tap;
use airplay_core::client::{
    begin_live_buffered_synchronized, begin_live_realtime_synchronized, BufferedAudioFormat,
    Client,
};
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

/// Buffered PCM24 stereo @ 48 kHz: 352 × 2 × 3.
const FRAME_BYTES_BUFFERED: usize = 352 * 2 * 3;
/// Realtime PCM16 stereo @ 48 kHz: 352 × 2 × 2 (big-endian L16).
const FRAME_BYTES_REALTIME: usize = 352 * 2 * 2;

/// ~0.7 s of frames at 48 kHz.
const PCM_CHANNEL_CAP: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AirplayTransport {
    Realtime,
    Buffered,
}

impl AirplayTransport {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "buffered" | "buffer" | "ptp" | "103" => Self::Buffered,
            _ => Self::Realtime,
        }
    }

    fn frame_bytes(self) -> usize {
        match self {
            Self::Realtime => FRAME_BYTES_REALTIME,
            Self::Buffered => FRAME_BYTES_BUFFERED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Buffered => "buffered",
        }
    }
}

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
    active: Arc<AtomicBool>,
    worker: Mutex<Option<WorkerHandle>>,
    shared: Arc<RwLock<SharedState>>,
    residual: Mutex<Vec<u8>>,
    wire_rate: u32,
    wire_bits: u16,
    channels: u16,
    transport: AirplayTransport,
}

impl AirplayManager {
    pub fn new(wire_rate: u32, wire_bits: u16, channels: u16, airplay_mode: &str) -> Self {
        let transport = AirplayTransport::parse(airplay_mode);
        Self {
            active: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            shared: Arc::new(RwLock::new(SharedState {
                sessions: Vec::new(),
            })),
            residual: Mutex::new(Vec::with_capacity(transport.frame_bytes() * 2)),
            wire_rate,
            wire_bits,
            channels: channels.max(1),
            transport,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn transport_label(&self) -> &'static str {
        self.transport.label()
    }

    /// Start AirPlay sessions (non-blocking spawn).
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
                    note: format!("connecting… ({})", self.transport.label()),
                })
                .collect();
        }
        self.residual.lock().clear();
        self.active.store(false, Ordering::Relaxed);

        let (tx, rx) = mpsc::sync_channel::<WorkerMsg>(PCM_CHANNEL_CAP);
        let shared = self.shared.clone();
        let active = self.active.clone();
        let transport = self.transport;

        let join = thread::Builder::new()
            .name("airplay-send".into())
            .spawn(move || {
                worker_main(rx, targets, volume_db as f64, shared, active, transport);
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
            if s.note.starts_with("live") || s.note.starts_with("connecting") {
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

    /// Feed capture wire PCM; converts to the active transport frame format.
    pub fn push_pcm(&self, wire: &[u8]) {
        if !self.is_active() || wire.is_empty() {
            return;
        }
        // 48 kHz PCM24 LE first (stream_tap path).
        let pcm24 =
            stream_tap::wire_to_stream_pcm(wire, self.wire_bits, self.wire_rate, self.channels);
        if pcm24.is_empty() {
            return;
        }
        let packed = match self.transport {
            AirplayTransport::Buffered => pcm24,
            AirplayTransport::Realtime => pcm24_le_to_pcm16_be(&pcm24),
        };
        if packed.is_empty() {
            return;
        }

        let frame_bytes = self.transport.frame_bytes();
        let mut residual = self.residual.lock();
        residual.extend_from_slice(&packed);

        let tx = {
            let guard = self.worker.lock();
            match guard.as_ref() {
                Some(w) => w.tx.clone(),
                None => return,
            }
        };

        while residual.len() >= frame_bytes {
            let frame: Vec<u8> = residual.drain(..frame_bytes).collect();
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
        Self::new(48_000, 24, 2, "realtime")
    }
}

/// PCM24 little-endian interleaved → PCM16 big-endian (realtime L16).
fn pcm24_le_to_pcm16_be(pcm24: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm24.len() / 3 * 2);
    for c in pcm24.chunks_exact(3) {
        let raw = c[0] as i32 | ((c[1] as i32) << 8) | ((c[2] as i8 as i32) << 16);
        let s16 = (raw >> 8).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        out.extend_from_slice(&s16.to_be_bytes());
    }
    out
}

// ---- worker ----------------------------------------------------------------

struct LiveClient {
    name: String,
    client: Client,
    /// Realtime: packets since last NTP sync.
    packets: u32,
}

fn worker_main(
    rx: mpsc::Receiver<WorkerMsg>,
    targets: Vec<SpeakerTarget>,
    volume_db: f64,
    shared: Arc<RwLock<SharedState>>,
    active: Arc<AtomicBool>,
    transport: AirplayTransport,
) {
    let groups = group_targets(&targets);
    let mut clients: Vec<LiveClient> = Vec::new();

    for group in groups {
        let res = match transport {
            AirplayTransport::Realtime => setup_and_begin_realtime(&group, volume_db, &shared),
            AirplayTransport::Buffered => {
                let format = BufferedAudioFormat::pcm_48000_24_stereo(true);
                setup_and_begin_buffered(&group, volume_db, &format, &shared)
            }
        };
        match res {
            Ok(group_live) => clients.extend(group_live),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    n = group.len(),
                    mode = transport.label(),
                    "airplay: group failed"
                );
                for t in &group {
                    let mut s = shared.write();
                    if let Some(row) = s.sessions.iter_mut().find(|r| r.name == t.name) {
                        if !row.active && row.note.starts_with("connecting") {
                            row.note = format!("error: {e:#}");
                        }
                    }
                }
            }
        }
    }

    if clients.is_empty() {
        active.store(false, Ordering::Relaxed);
        tracing::error!(mode = transport.label(), "airplay: no live clients");
        while let Ok(msg) = rx.recv() {
            if matches!(msg, WorkerMsg::Stop) {
                break;
            }
        }
        return;
    }

    active.store(true, Ordering::Relaxed);
    tracing::info!(
        n = clients.len(),
        mode = transport.label(),
        "airplay: live sessions ready"
    );

    // ~1 sync/s for realtime (352 samples @ 48 kHz ≈ 136 pkt/s).
    const REALTIME_SYNC_EVERY: u32 = 136;
    let mut first_frame = true;

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WorkerMsg::Frame(frame)) => {
                let marker = first_frame;
                first_frame = false;
                clients.retain_mut(|c| {
                    let write = match transport {
                        AirplayTransport::Buffered => c.client.write_buffered_frame(&frame),
                        AirplayTransport::Realtime => {
                            let r = c.client.write_realtime_frame(&frame, marker);
                            if r.is_ok() {
                                c.packets = c.packets.wrapping_add(1);
                                if c.packets % REALTIME_SYNC_EVERY == 0 {
                                    let _ = c.client.send_realtime_sync(false);
                                }
                            }
                            r
                        }
                    };
                    match write {
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
    tracing::info!(mode = transport.label(), "airplay: worker stopped");
}

fn update_session(shared: &RwLock<SharedState>, name: &str, active: bool, note: &str) {
    let mut s = shared.write();
    if let Some(row) = s.sessions.iter_mut().find(|r| r.name == name) {
        row.active = active;
        row.note = note.to_string();
    }
}

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
    for (_tsid, mut group) in by_tsid {
        if group.len() >= 2 {
            group.sort_by(|a, b| {
                let ra = stereo_rank(&a.name);
                let rb = stereo_rank(&b.name);
                ra.cmp(&rb).then_with(|| a.name.cmp(&b.name))
            });
            tracing::info!(
                n = group.len(),
                leader = %group[0].name,
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

fn stereo_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n.contains("(2)") || n.ends_with(" 2") || n.contains("right") {
        1
    } else {
        0
    }
}

// ---- realtime ---------------------------------------------------------------

fn setup_and_begin_realtime(
    group: &[SpeakerTarget],
    volume_db: f64,
    shared: &RwLock<SharedState>,
) -> anyhow::Result<Vec<LiveClient>> {
    if group.is_empty() {
        anyhow::bail!("empty group");
    }

    let mut staged: Vec<LiveClient> = Vec::new();
    let mut leader_uuid = String::new();

    for (i, t) in group.iter().enumerate() {
        update_session(shared, &t.name, false, "connecting… (realtime)");
        let dev = device_from_target(t);
        let mut client = Client::new();
        client.set_startup_volume(volume_db);

        if i > 0 && !leader_uuid.is_empty() {
            client.set_group_uuid(&leader_uuid);
        }

        if let Err(e) = client.connect(&dev, HOMEPOD_PIN) {
            tracing::error!(name = %t.name, ip = %t.ip, error = %e, "airplay: connect failed");
            update_session(shared, &t.name, false, &format!("error: connect {e:#}"));
            continue;
        }

        // setup_realtime already RECORD + FLUSH + first sync (no PTP wait).
        if let Err(e) = client.setup_realtime(48_000, 2) {
            tracing::error!(name = %t.name, error = %e, "airplay: setup_realtime failed");
            let _ = client.stop();
            update_session(shared, &t.name, false, &format!("error: setup {e:#}"));
            continue;
        }

        if i == 0 || leader_uuid.is_empty() {
            leader_uuid = client.group_uuid().to_string();
        }

        tracing::info!(
            name = %t.name,
            ip = %t.ip,
            "airplay: connected (realtime pcm16-48)"
        );
        staged.push(LiveClient {
            name: t.name.clone(),
            client,
            packets: 0,
        });
    }

    if staged.is_empty() {
        anyhow::bail!("all speakers in group failed connect/setup");
    }

    if staged.len() == 1 {
        update_session(shared, &staged[0].name, true, "live (realtime)");
        return Ok(staged);
    }

    // Align RTP clocks across the pair (no PTP).
    let names: Vec<String> = staged.iter().map(|c| c.name.clone()).collect();
    let mut raw: Vec<Client> = staged.into_iter().map(|c| c.client).collect();
    match begin_live_realtime_synchronized(&mut raw) {
        Ok(()) => {
            let mut out = Vec::with_capacity(raw.len());
            for (name, client) in names.into_iter().zip(raw.into_iter()) {
                update_session(shared, &name, true, "live (realtime)");
                out.push(LiveClient {
                    name,
                    client,
                    packets: 0,
                });
            }
            Ok(out)
        }
        Err(e) => {
            tracing::warn!(error = %e, "airplay: realtime sync failed; keeping leader only");
            let mut out = Vec::new();
            let mut iter = names.into_iter().zip(raw.into_iter());
            if let Some((name, client)) = iter.next() {
                update_session(shared, &name, true, "live (realtime, pair partial)");
                out.push(LiveClient {
                    name,
                    client,
                    packets: 0,
                });
            }
            for (name, mut client) in iter {
                let _ = client.stop();
                update_session(shared, &name, false, &format!("error: pair sync {e:#}"));
            }
            if out.is_empty() {
                Err(anyhow::anyhow!("realtime begin failed: {e:#}"))
            } else {
                Ok(out)
            }
        }
    }
}

// ---- buffered ---------------------------------------------------------------

fn setup_and_begin_buffered(
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
        update_session(shared, &t.name, false, "connecting… (buffered)");
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
            packets: 0,
        });
    }

    if staged.is_empty() {
        anyhow::bail!("all speakers in group failed connect/setup");
    }

    if staged.len() == 1 {
        let lc = &mut staged[0];
        lc.client
            .begin_buffered_playback()
            .map_err(|e| anyhow::anyhow!("{}: begin: {e:#}", lc.name))?;
        update_session(shared, &lc.name, true, "live (buffered)");
        return Ok(staged);
    }

    let names: Vec<String> = staged.iter().map(|c| c.name.clone()).collect();
    let mut raw: Vec<Client> = staged.into_iter().map(|c| c.client).collect();

    match begin_live_buffered_synchronized(&mut raw) {
        Ok(()) => {
            let mut out = Vec::with_capacity(raw.len());
            for (name, client) in names.into_iter().zip(raw.into_iter()) {
                update_session(shared, &name, true, "live (buffered)");
                out.push(LiveClient {
                    name,
                    client,
                    packets: 0,
                });
            }
            Ok(out)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "airplay: synchronized begin failed; keeping leader only"
            );
            let mut out = Vec::new();
            let mut iter = names.into_iter().zip(raw.into_iter());
            if let Some((name, mut client)) = iter.next() {
                match client.begin_buffered_playback() {
                    Ok(()) => {
                        update_session(
                            shared,
                            &name,
                            true,
                            "live (buffered, pair mate failed SETRATE)",
                        );
                        out.push(LiveClient {
                            name,
                            client,
                            packets: 0,
                        });
                    }
                    Err(e2) => {
                        tracing::error!(name = %name, error = %e2, "airplay: leader begin failed");
                        let _ = client.stop();
                        update_session(shared, &name, false, &format!("error: begin {e2:#}"));
                    }
                }
            }
            for (name, mut client) in iter {
                let _ = client.stop();
                update_session(
                    shared,
                    &name,
                    false,
                    &format!("error: pair SETRATE {e:#}"),
                );
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
