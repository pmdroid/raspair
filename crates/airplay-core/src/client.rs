//! High-level AirPlay 2 client: connect + authenticate, set up a realtime or
//! buffered stream, and pace audio out to the receiver. This is the Rust
//! analogue of the Go `client.Client`, focused on driving a single device.

use crate::audio::{self, PcmEncoder};
use crate::discovery::Device;
use crate::pairing::{
    CredentialStore, FileStore, PairingError, PairingSession, PinChallenge, PinPairings, PinSource, StoreError, Stored,
};
use crate::protocol::HapPairing;
use crate::rtsp::{setup, Client as RtspClient};
use crate::stream::{self, BufferedStream, RealtimeStream, TimingServer};
use crate::timing::{PtpAnchor, PtpClock};
use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    Realtime = 96,
    Buffered = 103,
}

/// SETUP "ct" compression-type bitmask: 1=PCM, 2=ALAC, 4=AAC-LC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm = 1,
    Alac = 2,
    Aac = 4,
}

/// A buffered-mode audio configuration (codec + format + the SETUP fields it
/// implies).
#[derive(Debug, Clone)]
pub struct BufferedAudioFormat {
    pub name: &'static str,
    pub codec: AudioCodec,
    pub sample_rate: i32,
    pub channels: i32,
    pub bits_per_sample: i32,
    pub little_endian: bool,
    pub samples_per_frame: i32,
    pub audio_format: u64,
    pub asc: Option<Vec<u8>>,
}

impl BufferedAudioFormat {
    pub fn aac_44100_stereo() -> Self {
        Self {
            name: "aac44100",
            codec: AudioCodec::Aac,
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: 0,
            little_endian: false,
            samples_per_frame: 1024,
            audio_format: setup::AUDIO_FORMAT_AAC_44100_2,
            asc: Some(audio::AAC_LC_44100_STEREO_ASC.to_vec()),
        }
    }
    pub fn aac_48000_stereo() -> Self {
        Self {
            name: "aac48000",
            codec: AudioCodec::Aac,
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 0,
            little_endian: false,
            samples_per_frame: 1024,
            audio_format: setup::AUDIO_FORMAT_AAC_48000_2,
            asc: Some(audio::AAC_LC_48000_STEREO_ASC.to_vec()),
        }
    }
    pub fn pcm_48000_24_stereo(little_endian: bool) -> Self {
        Self {
            name: if little_endian { "pcm48000-24" } else { "pcm48000-24be" },
            codec: AudioCodec::Pcm,
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 24,
            little_endian,
            samples_per_frame: 352,
            audio_format: setup::AUDIO_FORMAT_PCM_48000_24_2,
            asc: None,
        }
    }
    pub fn alac_48000_24_stereo() -> Self {
        Self {
            name: "alac48000-24",
            codec: AudioCodec::Alac,
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 24,
            little_endian: false,
            samples_per_frame: 352,
            audio_format: setup::AUDIO_FORMAT_ALAC_48000_24_2,
            asc: Some(audio::alac_magic_cookie(352, 24, 2, 48000)),
        }
    }
}

fn group_audio_key_mode() -> String {
    std::env::var("AP2_GROUP_AUDIO_KEY").unwrap_or_else(|_| "group".to_string())
}

fn duration_env(name: &str, default_ms: u64) -> Duration {
    if let Ok(v) = std::env::var(name) {
        if let Ok(ms) = v.parse::<u64>() {
            if ms > 0 {
                return Duration::from_millis(ms);
            }
        }
    }
    Duration::from_millis(default_ms)
}

/// Default PTP playout lead for buffered SETRATEANCHORTIME.
/// ESP vinyl path uses 12.5 s so stereo followers have buffer headroom;
/// 4 s is too short and often yields SETRATE status -71976 on the mate.
const DEFAULT_ANCHOR_LEAD_MS: u64 = 12_500;

pub struct Client {
    device: Option<Device>,
    session: Option<PairingSession>,
    rtsp: Option<RtspClient>,
    shared_secret: Vec<u8>,
    local_ip: String,
    session_num: u32,
    rtsp_path: String,
    group_uuid: String,
    shk: Vec<u8>,
    volume_db: f64,
    group_mode: bool,
    group_key: Vec<u8>,
    fairplay_key: Vec<u8>,
    fairplay_m3: Vec<u8>,
    oracle_addr: Option<String>,
    fairplay_helper: Option<String>,
    fairplay_audio_helper: Option<String>,

    streamer: Option<RealtimeStream>,
    buffered: Option<BufferedStream>,
    buffered_id: i64,
    timing: Option<TimingServer>,
    ptp: Option<Arc<PtpClock>>,
    events: Option<EventChannel>,
    last_feedback: Instant,
    pub(crate) deadline: Option<(Instant, crate::session::Cancellation)>,
    credentials: Option<Arc<dyn CredentialStore>>,
    password: Option<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            device: None,
            session: None,
            rtsp: None,
            shared_secret: Vec::new(),
            local_ip: String::new(),
            session_num: 0,
            rtsp_path: String::new(),
            group_uuid: String::new(),
            shk: Vec::new(),
            volume_db: -30.0,
            group_mode: false,
            group_key: Vec::new(),
            fairplay_key: Vec::new(),
            fairplay_m3: Vec::new(),
            oracle_addr: None,
            fairplay_helper: None,
            fairplay_audio_helper: None,
            streamer: None,
            buffered: None,
            buffered_id: 0,
            timing: None,
            ptp: None,
            events: None,
            last_feedback: Instant::now(),
            deadline: None,
            credentials: None,
            password: None,
        }
    }

    pub fn set_credential_store(&mut self, store: Arc<dyn CredentialStore>) {
        self.credentials = Some(store);
    }

    pub fn set_password(&mut self, password: &str) {
        self.password = Some(password.to_string());
    }

    fn credential_store(&mut self) -> Result<Arc<dyn CredentialStore>> {
        if self.credentials.is_none() {
            self.credentials = Some(Arc::new(FileStore::open_default()?));
        }
        Ok(self.credentials.clone().unwrap())
    }

    fn dial_event(&self, addr: &str, port: u16) -> Result<TcpStream> {
        if self.deadline.is_none() { return Ok(TcpStream::connect((addr, port))?); }
        let target = std::net::SocketAddr::new(addr.parse()?, port);
        loop {
            match TcpStream::connect_timeout(&target, self.remaining(Duration::from_millis(20))?) {
                Ok(s) => return Ok(s),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {},
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn remaining(&self, duration: Duration) -> Result<Duration> {
        if let Some((end, cancel)) = &self.deadline {
            anyhow::ensure!(!cancel.is_cancelled(), "cancelled");
            return Ok(duration.min(end.checked_duration_since(Instant::now()).ok_or_else(|| anyhow!("deadline expired"))?));
        }
        Ok(duration)
    }

    fn wait(&self, duration: Duration) -> Result<()> {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            std::thread::sleep(self.remaining(end.saturating_duration_since(Instant::now()).min(Duration::from_millis(2)))?);
        }
        Ok(())
    }

    pub fn set_startup_volume(&mut self, db: f64) {
        self.volume_db = db.clamp(-144.0, 0.0);
    }

    /// The PTP clock this client started (buffered mode), so a second speaker
    /// in a stereo pair can join the same timing group.
    pub fn ptp_clock(&self) -> Option<Arc<PtpClock>> {
        self.ptp.clone()
    }

    /// Reuse another speaker's PTP clock. Call before `setup_buffered`.
    pub fn set_ptp_clock(&mut self, ptp: Arc<PtpClock>) {
        self.ptp = Some(ptp);
    }

    pub fn group_uuid(&self) -> &str {
        &self.group_uuid
    }

    /// Share the group UUID so both speakers SETUP into the same session.
    /// Call before `setup_realtime` / `setup_buffered`.
    pub fn set_group_uuid(&mut self, uuid: &str) {
        self.group_uuid = uuid.to_string();
    }

    /// Set a FairPlay oracle address (host:port). When set, `fairplay_setup`
    /// will delegate the sender-side FairPlay computation to a local oracle
    /// helper instead of replaying a captured (and session-mismatched) blob.
    /// See bin/fairplay_oracle.py.
    pub fn set_oracle(&mut self, addr: &str) {
        self.oracle_addr = Some(addr.to_string());
    }

    /// Set the path to a Doubletake `fp-m3` helper binary. When set,
    /// `fairplay_setup` will run it as a subprocess oracle to compute the
    /// session-specific M3 from the Apple TV's fresh M2 reply, instead of
    /// replaying a captured blob or using the Python MITM oracle.
    ///
    /// Build the helper from <https://github.com/omarroth/doubletake>
    /// (commit 5f0c233f412d0b0e6c21d735d2afed95816a732b verified) and place the
    /// binary at the given path. It must read a hex M2 from stdin and write a
    /// hex M3 to stdout.
    pub fn set_fairplay_helper(&mut self, path: &str) {
        self.fairplay_helper = Some(path.to_string());
    }

    /// Set the path to a Doubletake `fp-audio-key` helper binary. Required for
    /// `AP2_GROUP_AUDIO_KEY=fairplay`, which derives the 16-byte AES audio key
    /// from the FairPlay M3, ekey, and pair-verify shared secret. If not set,
    /// the code falls back to the same directory as the `fp-m3` helper with the
    /// filename `fp-audio-key`.
    pub fn set_fairplay_audio_helper(&mut self, path: &str) {
        self.fairplay_audio_helper = Some(path.to_string());
    }

    /// Persistent pair-setup with an Apple TV (saves a controller identity so
    /// future connects can pair-verify). HomePods don't need this — they pair
    /// transiently at [`Client::connect`] time.
    pub fn pair(dev: &Device, pin: &str) -> Result<()> {
        Self::pair_with_store(dev, pin, &FileStore::open_default()?)
    }

    pub fn pair_with_store(dev: &Device, pin: &str, store: &dyn CredentialStore) -> Result<()> {
        let route = dev.route();
        if route.hap_pairing()? != HapPairing::Persistent {
            return Err(crate::session::SessionError::UnsupportedAuthentication(route.auth).into());
        }
        let addr = dev.ip().ok_or_else(|| anyhow!("no IP address for device"))?;
        let mut rtsp = RtspClient::connect(&addr, dev.port).context("RTSP connect")?;
        let mut session = PairingSession::new(&dev.record.device_id, pin);
        session.load_or_create_identity(store)?;
        session.pair_apple_tv(&mut rtsp).context("pair-setup")?;
        let record = session.pairing_record().ok_or_else(|| anyhow!("pair-setup returned no accessory identity"))?;
        store.save(&record).context("save pairing")?;
        Ok(())
    }

    pub fn begin_pairing(dev: &Device, pairings: &PinPairings) -> Result<PinChallenge> {
        let route = dev.route();
        if route.hap_pairing()? != HapPairing::Persistent {
            return Err(crate::session::SessionError::UnsupportedAuthentication(route.auth).into());
        }
        let addr = dev.ip().ok_or_else(|| anyhow!("no IP address for device"))?;
        let rtsp = RtspClient::connect(&addr, dev.port).context("RTSP connect")?;
        let source = if dev.record.displays_pin() { PinSource::OnScreen } else { PinSource::Static };
        Ok(pairings.begin(&dev.record.device_id, source, Box::new(rtsp))?)
    }

    /// Dial the receiver, authenticate on that connection (transient
    /// pair-setup for HomePods, pair-verify for paired Apple TVs), and switch
    /// the connection to the AirPlay 2 channel cipher.
    pub fn connect(&mut self, dev: &Device, pin: &str) -> Result<()> {
        dev.route().hap_pairing()?;
        let addr = dev.ip().ok_or_else(|| anyhow!("no IP address for device"))?;
        let rtsp = RtspClient::connect(&addr, dev.port).context("RTSP connect")?;
        self.connect_transport(dev, pin, rtsp)
    }

    pub(crate) fn connect_transport(&mut self, dev: &Device, pin: &str, mut rtsp: RtspClient) -> Result<()> {
        let pairing = dev.route().hap_pairing()?;
        self.device = Some(dev.clone());
        let addr = dev.ip().ok_or_else(|| anyhow!("no IP address for device"))?;
        let mut session = PairingSession::new(&dev.record.device_id, pin);
        session.deadline = self.deadline.clone();

        if pairing == HapPairing::Transient {
            session.pair_homepod(&mut rtsp).context("pair HomePod")?;
            self.shared_secret = session.shared_secret.clone();
        } else if pairing == HapPairing::TransientPassword {
            let store = self.credential_store()?;
            session
                .pair_transient_password(&mut rtsp, self.password.as_deref(), store.as_ref())
                .context("transient pair-setup")?;
            self.shared_secret = session.shared_secret.clone();
        } else {
            match self.credential_store()?.load(&dev.record.device_id) {
                Ok(Stored::Paired(record)) => session.use_record(&record),
                Ok(Stored::Missing) => return Err(PairingError::NotPaired.into()),
                Ok(Stored::RePairRequired { .. }) => return Err(PairingError::LegacyKeyOnly.into()),
                Err(
                    StoreError::Malformed { .. } | StoreError::InvalidField { .. } | StoreError::UnsupportedVersion { .. },
                ) => return Err(PairingError::InvalidRecord.into()),
                Err(e) => return Err(e.into()),
            }
            session.pair_verify(&mut rtsp).context("pair-verify")?;
            self.shared_secret = session.shared_secret.clone();
        }

        rtsp.enable_encryption(&self.shared_secret)
            .context("enable channel encryption")?;

        let client_instance = rand_hex(8);
        rtsp.set_default_header("DACP-ID", &client_instance);
        rtsp.set_default_header("Active-Remote", &rand_u32().to_string());
        rtsp.set_default_header("Client-Instance", &client_instance);

        self.local_ip = rtsp.local_ip().unwrap_or_else(|| addr.clone());
        self.session_num = rand_u32();
        self.rtsp_path = format!("rtsp://{}/{}", self.local_ip, self.session_num);

        let info = rtsp.info().context("RTSP info")?;
        if info.status_code != 200 {
            return Err(crate::session::SessionError::ReceiverRejected { status: info.status_code as u16 }.into());
        }

        self.session = Some(session);
        self.rtsp = Some(rtsp);
        Ok(())
    }

    pub fn setup_realtime(&mut self, sample_rate: i32, _channels: i32) -> Result<()> {
        self.setup_stream(
            StreamType::Realtime,
            AudioCodec::Pcm,
            352,
            sample_rate,
            setup::AUDIO_FORMAT_PCM_44100_16_2,
            None,
        )
    }

    /// Experimental: set up a buffered stream into an **entertainment /
    /// home-theater group** by targeting the group leader (an Apple TV) with
    /// the multicast-group SETUP descriptor. The leader fans audio out to its
    /// bound members. See docs/ENTERTAINMENT_GROUPS.md.
    pub fn setup_group(&mut self, format: &BufferedAudioFormat) -> Result<()> {
        self.group_mode = true;
        self.setup_buffered(format)
    }

    pub fn setup_buffered(&mut self, format: &BufferedAudioFormat) -> Result<()> {
        if format.channels != 2 {
            bail!("buffered mode currently supports stereo only");
        }
        self.setup_stream(
            StreamType::Buffered,
            format.codec,
            format.samples_per_frame,
            format.sample_rate,
            format.audio_format,
            format.asc.clone(),
        )
    }

    fn rtsp_mut(&mut self) -> &mut RtspClient {
        self.rtsp.as_mut().expect("connected")
    }

    /// FairPlay `/fp-setup` attestation. An Apple TV leading an entertainment
    /// group requires this 2-message handshake before it will render audio
    /// (HomePods don't).
    ///
    /// Priority for deriving setup2 from the fresh M2 reply:
    /// 1. Doubletake `fp-m3` subprocess helper (`fairplay_helper`).
    /// 2. Python MITM oracle (`oracle_addr`, `bin/fairplay_oracle.py`).
    /// 3. Captured replay blob (usually fails with 403 because M2 is
    ///    session-random).
    fn fairplay_setup(&mut self) -> Result<()> {
        // Phase 1: send setup1, get the session-random 142-byte reply.
        let r1 = self
            .rtsp_mut()
            .do_request(setup::new_fp_setup(setup::FP_SETUP1))
            .context("fp-setup1")?;
        log::debug!("fp-setup1 → status={} ({} bytes)", r1.status_code, r1.body.len());
        if r1.status_code != 200 {
            bail!("fp-setup1 returned {}", r1.status_code);
        }
        let reply_hex: String = r1.body.iter().map(|b| format!("{b:02x}")).collect();
        log::debug!("fp-setup1 reply body: {reply_hex}");
        if r1.body.len() != 142 {
            log::warn!("fp-setup1 reply is {} bytes, expected 142; M3 derivation may fail", r1.body.len());
        }

        // Phase 2: get setup2 — helper > oracle > captured blob.
        let setup2: Vec<u8> = if let Some(ref helper) = self.fairplay_helper {
            log::info!("computing FairPlay setup2 via Doubletake helper: {helper}");
            compute_fairplay_setup2(helper, &r1.body)?
        } else if let Some(ref oracle) = self.oracle_addr {
            log::info!("computing FairPlay setup2 via MITM oracle: {oracle}");
            self.fairplay_setup_via_oracle(&r1.body, oracle)?
        } else {
            log::warn!("no helper/oracle — replaying captured FP_SETUP2 (will likely get 403)");
            setup::FP_SETUP2.to_vec()
        };

        // Save the M3 we sent; it is needed to derive the FairPlay audio key
        // (ekey/eiv path) if the group uses it.
        self.fairplay_m3 = setup2.clone();

        let r2 = self
            .rtsp_mut()
            .do_request(setup::new_fp_setup(&setup2))
            .context("fp-setup2")?;
        log::debug!("fp-setup2 → status={} ({} bytes)", r2.status_code, r2.body.len());
        if r2.status_code != 200 {
            log::warn!("fp-setup2 returned {} (continuing; leader may still gate audio)", r2.status_code);
        } else if r2.body.len() == 32 {
            self.fairplay_key = r2.body.clone();
            log::debug!("captured FairPlay session key ({})", hex::encode(&self.fairplay_key));
        }
        Ok(())
    }

    /// Ask the local FairPlay oracle to compute `f(reply) → setup2`.
    ///
    /// Protocol (line-based TCP):
    ///   → INJECT_REPLY <142-byte-hex>
    ///   ← OK reply armed
    ///   → WAITING
    ///   ← SETUP2 <164-byte-hex>
    fn fairplay_setup_via_oracle(&self, reply: &[u8], addr: &str) -> Result<Vec<u8>> {
        let stream = TcpStream::connect(addr)
            .with_context(|| format!("connect to oracle at {addr}"))?;
        let mut writer = stream.try_clone().context("clone oracle stream")?;
        let mut reader = BufReader::new(stream);

        // Send the ATV's reply to the oracle.
        let hex = reply.iter().map(|b| format!("{b:02x}")).collect::<String>();
        writeln!(writer, "INJECT_REPLY {hex}").context("send INJECT_REPLY to oracle")?;
        let mut ok = String::new();
        reader.read_line(&mut ok).context("read oracle OK")?;
        if !ok.trim().starts_with("OK") {
            bail!("oracle rejected reply: {ok}");
        }

        // Tell user to trigger Mac AirPlay, then wait for setup2.
        println!(
            "\n╔══════════════════════════════════════════════════════════════╗\n\
             ║  Sent ATV's reply to FairPlay oracle                        ║\n\
             ║  Now AirPlay audio from your Mac to the \"FairPlay Oracle\"  ║\n\
             ║  device. The oracle will capture the Mac's setup2.          ║\n\
             ╚══════════════════════════════════════════════════════════════╝"
        );
        writeln!(writer, "WAITING").context("send WAITING to oracle")?;
        let mut setup2_line = String::new();
        reader
            .read_line(&mut setup2_line)
            .context("read setup2 from oracle")?;

        let setup2_hex = setup2_line
            .trim()
            .strip_prefix("SETUP2 ")
            .ok_or_else(|| anyhow!("unexpected oracle response: {setup2_line}"))?;
        let setup2 = hex::decode(setup2_hex).context("decode oracle setup2 hex")?;
        if setup2.len() != 164 {
            bail!("oracle returned {} bytes, expected 164", setup2.len());
        }
        log::info!("Got valid setup2 from oracle ({} bytes)", setup2.len());
        Ok(setup2)
    }

    /// Derive the FairPlay audio key (fpKey) from the stored M3, a generated
    /// ekey, and the pair-verify shared secret. Returns (fpKey, ekey, eiv).
    ///
    /// This delegates the playfair AES table lookup to the Doubletake
    /// `fp-audio-key` helper so airplay-core stays MIT-licensed.
    fn fairplay_key_for_stream(&self) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let helper = if let Some(ref h) = self.fairplay_audio_helper {
            h.clone()
        } else if let Some(ref h) = self.fairplay_helper {
            // Default: same directory as fp-m3, named fp-audio-key.
            let p = PathBuf::from(h);
            let dir = p.parent().unwrap_or_else(|| std::path::Path::new("."));
            dir.join("fp-audio-key").to_string_lossy().to_string()
        } else {
            bail!("FairPlay audio key mode requires --fairplay-helper");
        };
        if self.fairplay_m3.len() != 164 {
            bail!("FairPlay M3 not available ({} bytes)", self.fairplay_m3.len());
        }

        let ekey = build_fairplay_ekey();
        let eiv = rand_key32_16();

        let m3_hex = hex::encode(&self.fairplay_m3);
        let ekey_hex = hex::encode(&ekey);
        let secret_hex = hex::encode(&self.shared_secret);

        let mut child = Command::new(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn FairPlay audio-key helper {helper}"))?;

        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("missing helper stdin"))?
            .write_all(format!("{m3_hex} {ekey_hex} {secret_hex}").as_bytes())
            .context("write to fp-audio-key helper")?;

        let output = child.wait_with_output().context("wait for fp-audio-key helper")?;
        if !output.status.success() {
            bail!("fp-audio-key failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let fp_key_hex = String::from_utf8(output.stdout).context("helper stdout is not UTF-8")?;
        let fp_key = hex::decode(fp_key_hex.trim()).context("decode fpKey hex")?;
        if fp_key.len() != 16 {
            bail!("fp-audio-key returned {} bytes, expected 16", fp_key.len());
        }
        log::info!("FairPlay audio key derived ({} bytes)", fp_key.len());
        Ok((fp_key, ekey, eiv))
    }

    fn setup_stream(
        &mut self,
        stream_type: StreamType,
        codec: AudioCodec,
        spf: i32,
        sample_rate: i32,
        mut audio_format: u64,
        asc: Option<Vec<u8>>,
    ) -> Result<()> {
        let dev = self.device.clone().ok_or_else(|| anyhow!("not connected"))?;
        let addr = dev.ip().ok_or_else(|| anyhow!("device has no IP"))?;

        let mut timing_protocol = "NTP";
        let mut timing_port = 0i64;
        let mut local_addresses: Vec<String> = Vec::new();

        if stream_type == StreamType::Buffered {
            timing_protocol = "PTP";
            if !self.local_ip.is_empty() {
                local_addresses.push(self.local_ip.clone());
            }
        } else {
            let ts = TimingServer::start().context("start timing server")?;
            timing_port = ts.port() as i64;
            self.timing = Some(ts);
        }

        let session_uuid = if self.group_uuid.is_empty() {
            let u = rand_uuid();
            self.group_uuid = u.clone();
            u
        } else {
            self.group_uuid.clone()
        };

        // FairPlay attestation must precede SETUP on ATV — the ATV gates
        // audio rendering on it. Run it always, falling back to the captured
        // blob when no oracle is available.
        if let Err(e) = self.fairplay_setup() {
            log::warn!("fp-setup failed (continuing to SETUP anyway): {e}");
        }

        // PTP is needed for any buffered stream, not just group mode.
        if stream_type == StreamType::Buffered && self.ptp.is_none() {
            if let Some(v4) = dev.ipv4 {
                match PtpClock::start(v4) {
                    Ok(ptp) => self.ptp = Some(Arc::new(ptp)),
                    Err(e) => log::warn!("PTP failed to start (buffered anchor likely rejected): {e}"),
                }
            }
        }

        // Decide early which audio key the group-unicast fallback will use so
        // that FairPlay ekey/eiv can be derived before the group SETUP.
        let key_mode = if self.group_mode {
            group_audio_key_mode()
        } else {
            "shk".to_string()
        };
        let mut fairplay_eiv: Option<Vec<u8>> = None;

        // Entertainment-group mode targets the leader (Apple TV) and carries the
        // multicast group descriptor + hijackID; the leader fans audio out to its
        // bound members. Otherwise this is the normal per-device base SETUP.
        let base_req = if self.group_mode && stream_type == StreamType::Buffered {
            let clock_id = self.ptp.as_ref().map(|p| p.clock_id() as i64).unwrap_or(0);
            let group_key = rand_key32();
            self.group_key = group_key.clone();

            // Optional FairPlay ekey/eiv/et in the group SETUP. The native macOS
            // sender includes these when it uses the FairPlay audio path; we add
            // them for AP2_GROUP_EKEY=1 or when the key mode is fairplay (which
            // needs the derived fpKey to encrypt audio). AES-CBC framing is only
            // used in fairplay mode; other modes keep ChaCha20-Poly1305 framing
            // even when ekey/eiv are present for testing.
            let include_ekey = key_mode == "fairplay"
                || std::env::var("AP2_GROUP_EKEY").unwrap_or_default() == "1";
            let (et, ekey, eiv) = if include_ekey {
                (Some(32u64), Some(build_fairplay_ekey()), Some(rand_key32_16()))
            } else {
                (None, None, None)
            };
            if key_mode == "fairplay" {
                fairplay_eiv = eiv.clone();
            }

            let params = setup::GroupSetupParams {
                device_id: rand_mac(),
                mac_address: rand_mac(),
                name: "airplay-rs".to_string(),
                local_addresses: local_addresses.clone(),
                clock_id,
                group_uuid: session_uuid.clone(),
                session_uuid: rand_uuid(),
                session_correlation_uuid: rand_uuid(),
                hijack_id: rand_uuid(),
                source_ipv4: self.local_ip.clone(),
                group_ipv4: rand_group_ipv4(),
                group_ipv6: Some(rand_group_ipv6()),
                source_ipv6: None,
                group_encryption_key: group_key,
                et,
                ekey,
                eiv,
            };
            log::debug!(
                "group SETUP → leader={} groupUUID={} groupIPv4={} clockID={} ekey={}",
                dev.name,
                params.group_uuid,
                params.group_ipv4,
                clock_id,
                params.ekey.as_ref().map(|v| v.len()).unwrap_or(0)
            );
            setup::new_group_session_setup(&self.rtsp_path, &params)?
        } else {
            setup::new_base_setup(
                &self.rtsp_path,
                &dev.record.device_id,
                &session_uuid,
                timing_protocol,
                timing_port,
                &local_addresses,
            )?
        };
        let base_resp = self.rtsp_mut().do_request(base_req).context("session SETUP")?;
        if base_resp.status_code != 200 {
            return Err(crate::session::SessionError::ReceiverRejected { status: base_resp.status_code as u16 }.into());
        }
        let base = setup::parse_setup_base_response(&base_resp).context("parse base SETUP")?;
        log::debug!(
            "base SETUP ok eventPort={} timingPort={}",
            base.event_port,
            base.timing_port
        );

        if base.event_port > 0 {
            match self.dial_event(&addr, base.event_port as u16).and_then(|stream| EventChannel::start(stream, &self.shared_secret)) {
                Ok(ev) => self.events = Some(ev),
                Err(e) => log::warn!("event channel failed (continuing): {e}"),
            }
        }

        if stream_type == StreamType::Buffered {
            if self.ptp.is_none() {
                if let Some(v4) = dev.ipv4 {
                    match PtpClock::start(v4) {
                        Ok(ptp) => self.ptp = Some(Arc::new(ptp)),
                        Err(e) => log::warn!("PTP failed to start (buffered anchor likely rejected): {e}"),
                    }
                }
            }
        }

        // Audio encryption key: HKDF-derived from the pair-verify shared secret
        // using the event-channel salt (matching pyatv / Apple's own sender).
        // Group mode can use one of four keys for the leader-unicast fallback:
        //   group    = the 32-byte groupEncryptionKey from the SETUP descriptor
        //   shk      = normal HKDF-derived shk (Events-Salt + Write key)
        //   raw      = first 32 bytes of the pair-verify shared secret (matches
        //              the Go/C reference implementation)
        //   fairplay = FairPlay-derived AES key from ekey/eiv + pair-verify secret
        if self.group_mode && key_mode == "fairplay" {
            let (fp_key, _ekey, eiv) = self.fairplay_key_for_stream()?;
            self.shk = fp_key;
            fairplay_eiv = Some(eiv);
            log::debug!("group mode: keying audio with FairPlay-derived fpKey");
        } else if self.group_mode && key_mode == "shk" {
            self.shk = crate::pairing::crypto::hkdf_expand(
                &self.shared_secret,
                b"Events-Salt",
                "Events-Write-Encryption-Key",
                32,
            );
            log::debug!("group mode: keying audio with HKDF-derived shk");
        } else if self.group_mode && key_mode == "raw" {
            self.shk = self.shared_secret[..32.min(self.shared_secret.len())].to_vec();
            log::debug!("group mode: keying audio with raw shared secret");
        } else if self.group_mode && !self.group_key.is_empty() {
            self.shk = self.group_key.clone();
            log::debug!("group mode: keying audio with groupEncryptionKey");
        } else {
            self.shk = crate::pairing::crypto::hkdf_expand(
                &self.shared_secret,
                b"Events-Salt",
                "Events-Write-Encryption-Key",
                32,
            );
            log::debug!("keying audio with HKDF-derived shk (Events-Salt + Write key)");
        }

        if stream_type == StreamType::Realtime && codec == AudioCodec::Pcm && sample_rate == 48000 {
            audio_format = setup::AUDIO_FORMAT_PCM_48000_16_2;
        }

        let mut control_streamer = RealtimeStream::new(&self.shk, &addr, sample_rate, spf, self.session_num)?;
        control_streamer.set_payload_type(stream_type as i32);
        let local_control_port = control_streamer.local_control_port() as i64;
        let base_seq = control_streamer.seq();
        let base_rtp = control_streamer.rtp_timestamp();

        let stream_req = setup::new_stream_setup(
            &self.rtsp_path,
            &self.shk,
            asc.as_deref(),
            stream_type as i64,
            spf as i64,
            sample_rate as i64,
            local_control_port,
            audio_format,
            codec as i64,
            self.session_num as u64,
            None,
            None,
        )?;
        let stream_resp = self.rtsp_mut().do_request(stream_req).context("stream SETUP")?;
        if stream_resp.status_code != 200 {
            return Err(crate::session::SessionError::ReceiverRejected { status: stream_resp.status_code as u16 }.into());
        }
        let sr = setup::parse_setup_response(&stream_resp)?;
        let s = setup::find_stream(&sr, stream_type as i64)?;
        let mut data_port = s.data_port;
        let control_port = s.control_port;
        if self.group_mode {
            if let Ok(override_port) = std::env::var("AP2_GROUP_DATA_PORT") {
                if let Ok(p) = override_port.parse::<i64>() {
                    log::debug!(
                        "group mode: overriding dataPort {} with AP2_GROUP_DATA_PORT={}",
                        data_port,
                        p
                    );
                    data_port = p;
                }
            }
        }
        log::debug!(
            "stream SETUP ok streamID={} dataPort={} controlPort={}",
            s.stream_id,
            data_port,
            control_port
        );

        if stream_type == StreamType::Buffered {
            let ssrc = stream::buffered_ssrc_for_audio_format(audio_format)?;
            self.buffered_id = s.stream_id;
            let mut buffered = if let Some(ref iv) = fairplay_eiv {
                BufferedStream::new_with_aes_iv(&self.shk, iv, &addr, data_port, control_port, sample_rate, spf, ssrc)
            } else {
                BufferedStream::new(&self.shk, &addr, data_port, control_port, sample_rate, spf, ssrc)
            };
            buffered.set_clock(base_seq as u32, base_rtp);
            self.buffered = Some(buffered);

            // Real senders send SETPEERS between the stream SETUP and RECORD;
            // without it the receiver may refuse the PTP timing group.
            let mut peers = vec![addr.clone()];
            if !self.local_ip.is_empty() {
                peers.push(self.local_ip.clone());
            }
            match setup::new_set_peers(&self.rtsp_path, &peers) {
                Ok(req) => {
                    let _ = self.rtsp_mut().do_request(req);
                }
                Err(e) => log::warn!("SETPEERS build failed (continuing): {e}"),
            }
        } else {
            control_streamer.start(data_port, control_port).context("start stream")?;
            self.streamer = Some(control_streamer);
        }

        let vol = self.volume_db;
        if let Err(e) = self.set_volume(vol) {
            log::warn!("set volume failed (continuing): {e}");
        }

        let send_record = stream_type == StreamType::Realtime
            || (self.group_mode
                && stream_type == StreamType::Buffered
                && std::env::var("AP2_GROUP_RECORD").unwrap_or_default() == "1");

        if send_record {
            let (seq, rtptime) = if stream_type == StreamType::Realtime {
                let s = self.streamer.as_ref().unwrap();
                (s.seq() as u32, s.rtp_timestamp())
            } else {
                let b = self.buffered.as_ref().unwrap();
                (b.seq(), b.rtp_timestamp())
            };
            let path = self.rtsp_path.clone();
            let record = self.rtsp_mut().do_request(setup::new_record(&path)).context("record")?;
            if record.status_code != 200 {
                bail!("record returned {}", record.status_code);
            }
            let rtp_info = format!("seq={seq};rtptime={rtptime}");
            let _ = self.rtsp_mut().do_request(setup::new_flush(&path, &rtp_info));
            if stream_type == StreamType::Realtime {
                self.streamer.as_mut().unwrap().send_sync(true).context("first sync")?;
            }
        }

        Ok(())
    }

    /// RECORD / FLUSHBUFFERED / SETRATEANCHORTIME to arm buffered playback at
    /// the PTP anchor.
    fn start_buffered_playback(&mut self, seq: u32, rtp_time: u32, shared_anchor: Option<PtpAnchor>) -> Result<()> {
        let anchor_lead = duration_env("AP2_ANCHOR_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS);

        if let Some(ptp) = self.ptp.as_ref() {
            log::debug!(
                "buffered playback: PTP ready={} delay_resp={} gm_clock={:#x}",
                ptp.ready(),
                ptp.delay_responses(),
                ptp.anchor_after(anchor_lead).map(|a| a.timeline_id).unwrap_or(0)
            );
        } else {
            log::warn!("buffered playback: no PTP clock — anchor will be zero (likely silent)");
        }

        // Stereo mates must keep the *same* absolute network-time anchor on every
        // retry (ESP airplay_start_playback). Regenerating on attempt>1 desyncs
        // the follower and often yields SETRATE status -71976.
        let fixed_shared = shared_anchor.is_some();
        let mut anchor = if let Some(a) = shared_anchor {
            a
        } else if let Some(ptp) = self.ptp.as_ref() {
            let end = Instant::now() + Duration::from_secs(8);
            while !ptp.ready() && Instant::now() < end { self.wait(Duration::from_millis(2))?; }
            let ready = ptp.ready();
            if !ready {
                log::warn!("PTP did not lock before buffered anchor (Delay_Resp={})", ptp.delay_responses());
            }
            ptp.anchor_after(anchor_lead).unwrap_or_default()
        } else {
            PtpAnchor::default()
        };
        log::debug!(
            "buffered anchor: timeline={:#x} secs={} frac={} seq={} rtpTime={} shared={}",
            anchor.timeline_id,
            anchor.seconds,
            anchor.fraction,
            seq,
            rtp_time,
            fixed_shared
        );

        let record = setup::new_record_buffered(&self.rtsp_path, self.buffered_id, seq, rtp_time)?;
        let record_resp = self.rtsp_mut().do_request(record).context("record buffered")?;
        if record_resp.status_code != 200 {
            bail!("record buffered returned {}", record_resp.status_code);
        }

        // FLUSHBUFFERED: stereo followers can reject briefly while tight-sync
        // settles — retry like ESP (up to 5 × 400 ms).
        let flush_until = (seq.wrapping_sub(1)) & 0xFF_FFFF;
        let mut flush_ok = false;
        for attempt in 1..=5 {
            let flush =
                setup::new_flush_buffered(&self.rtsp_path, 0, rtp_time, flush_until, rtp_time)?;
            let flush_resp = self.rtsp_mut().do_request(flush).context("flush buffered")?;
            if flush_resp.status_code == 200 {
                flush_ok = true;
                break;
            }
            log::warn!(
                "FLUSHBUFFERED returned {} attempt {attempt}/5",
                flush_resp.status_code
            );
            self.wait(Duration::from_millis(400))?;
        }
        if !flush_ok {
            bail!("flush buffered rejected after retries");
        }

        let mut last_status = 0i64;
        for attempt in 1..=5 {
            // Only refresh anchor for single-speaker (no shared) sessions.
            if !fixed_shared {
                if let Some(ptp) = self.ptp.as_ref() {
                    if let Some(a) = ptp.anchor_after(anchor_lead) {
                        anchor = a;
                    }
                }
            }
            let rate = setup::new_set_rate_anchor_time(
                &self.rtsp_path,
                rtp_time,
                anchor.timeline_id,
                anchor.seconds,
                anchor.fraction,
            )?;
            let rate_resp = self.rtsp_mut().do_request(rate).context("set rate anchor time")?;
            if rate_resp.status_code != 200 {
                bail!("set rate anchor time returned {}", rate_resp.status_code);
            }
            let (status, has_status) = setup::parse_status(&rate_resp)?;
            if !has_status || status == 0 {
                log::debug!("SETRATEANCHORTIME accepted (attempt {attempt}, status={status})");
                return Ok(());
            }
            last_status = status;
            log::warn!("SETRATEANCHORTIME status {status} on attempt {attempt}");
            self.wait(Duration::from_secs(1))?;
        }
        bail!("set rate anchor time rejected with status {last_status}")
    }

    /// Stream realtime PCM at wall-clock pace with periodic sync + feedback.
    pub fn play_pcm(&mut self, samples: &[f32], sample_rate: i32, channels: i32) -> Result<()> {
        play_pcm_synchronized(std::slice::from_mut(self), samples, sample_rate, channels)
    }

    pub fn play_buffered_pcm(&mut self, samples: &[f32], sample_rate: i32, channels: i32, bits: i32, little_endian: bool) -> Result<()> {
        let spf = self.buffered.as_ref().ok_or_else(|| anyhow!("buffered stream not set up"))?.samples_per_frame() as usize;
        let enc = PcmEncoder::new(sample_rate, channels, bits, little_endian);
        let frame_samples = spf * channels as usize;
        if samples.len() < frame_samples {
            bail!("not enough samples for one buffered PCM frame");
        }
        let mut frames = Vec::with_capacity(samples.len() / frame_samples);
        let mut i = 0;
        while i + frame_samples <= samples.len() {
            frames.push(enc.encode(&samples[i..i + frame_samples]));
            i += frame_samples;
        }
        self.play_buffered_frames(frames)
    }

    pub fn play_aac_frames(&mut self, frames: Vec<Vec<u8>>, _sample_rate: i32) -> Result<()> {
        if self.buffered.is_none() {
            bail!("buffered stream not set up");
        }
        self.play_buffered_frames(frames)
    }

    pub fn play_buffered_alac_escape(&mut self, samples: &[f32]) -> Result<()> {
        let spf = self
            .buffered
            .as_ref()
            .ok_or_else(|| anyhow!("buffered stream not set up"))?
            .samples_per_frame() as usize;
        let frame_samples = spf * 2;
        let mut frames = Vec::with_capacity(samples.len() / frame_samples);
        let mut i = 0;
        while i + frame_samples <= samples.len() {
            frames.push(audio::encode_alac_escape_frame(&samples[i..i + frame_samples], spf));
            i += frame_samples;
        }
        self.play_buffered_frames(frames)
    }

    pub fn play_buffered_alac_frames(&mut self, frames: Vec<Vec<u8>>, _sample_rate: i32) -> Result<()> {
        if self.buffered.is_none() {
            bail!("buffered stream not set up");
        }
        self.play_buffered_frames(frames)
    }

    fn play_buffered_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<()> {
        play_buffered_frames_synchronized(std::slice::from_mut(self), frames)
    }

    pub fn set_volume(&mut self, db: f64) -> Result<()> {
        if self.rtsp.is_none() || self.rtsp_path.is_empty() {
            bail!("not connected");
        }
        let db = db.clamp(-144.0, 0.0);
        self.volume_db = db;
        let path = self.rtsp_path.clone();
        let req = setup::new_set_parameter(&path, "volume", &format!("{db}"));
        let resp = self.rtsp_mut().do_request(req).context("set volume")?;
        if resp.status_code != 200 {
            bail!("set volume returned {}", resp.status_code);
        }
        Ok(())
    }

    /// Whether a buffered stream is configured (after `setup_buffered`).
    pub fn has_buffered(&self) -> bool {
        self.buffered.is_some()
    }

    /// Samples per frame for the buffered stream (typically 352 for PCM24).
    pub fn samples_per_frame(&self) -> Option<i32> {
        self.buffered.as_ref().map(|b| b.samples_per_frame())
    }

    /// Align this client's buffered RTP clock to `other` (stereo pair lockstep).
    /// Call after both have `setup_buffered`, before `begin_buffered_playback*`.
    pub fn sync_buffered_clock_from(&mut self, other: &Client) -> Result<()> {
        let (seq, rtp) = {
            let b = other
                .buffered
                .as_ref()
                .ok_or_else(|| anyhow!("other buffered stream not set up"))?;
            (b.seq(), b.rtp_timestamp())
        };
        let b = self
            .buffered
            .as_mut()
            .ok_or_else(|| anyhow!("buffered stream not set up"))?;
        b.set_clock(seq, rtp);
        Ok(())
    }

    /// Wait for PTP lock and build a shared anchor for stereo mates / live play.
    pub fn ptp_anchor_for_playback(&self) -> Option<PtpAnchor> {
        let lead = duration_env("AP2_ANCHOR_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS);
        let ptp = self.ptp.as_ref()?;
        ptp.wait_ready(Duration::from_secs(8));
        ptp.anchor_after(lead)
    }

    /// Arm RECORD / FLUSHBUFFERED / SETRATEANCHORTIME for continuous live frames.
    /// Call once after `setup_buffered`, then feed with [`Client::write_buffered_frame`].
    pub fn begin_buffered_playback(&mut self) -> Result<()> {
        let (seq, rtp) = {
            let b = self
                .buffered
                .as_ref()
                .ok_or_else(|| anyhow!("buffered stream not set up"))?;
            (b.seq(), b.rtp_timestamp())
        };
        self.start_buffered_playback(seq, rtp, None)
    }

    /// Like [`Client::begin_buffered_playback`] but reuses a PTP anchor from the
    /// stereo-pair leader so both speakers map the same rtpTime to network time.
    pub fn begin_buffered_playback_with_anchor(
        &mut self,
        shared_anchor: Option<PtpAnchor>,
    ) -> Result<()> {
        let (seq, rtp) = {
            let b = self
                .buffered
                .as_ref()
                .ok_or_else(|| anyhow!("buffered stream not set up"))?;
            (b.seq(), b.rtp_timestamp())
        };
        self.start_buffered_playback(seq, rtp, shared_anchor)
    }

    /// Encrypt and TCP-write one pre-encoded buffered audio frame (e.g. 352×2×3
    /// bytes of little-endian PCM24). Safe to call from a dedicated sender
    /// thread after [`Client::begin_buffered_playback`].
    pub fn write_buffered_frame(&mut self, payload: &[u8]) -> Result<()> {
        let b = self
            .buffered
            .as_mut()
            .ok_or_else(|| anyhow!("buffered stream not set up"))?;
        b.write_frame(payload, false)
            .context("write buffered frame")?;
        feedback_if_due(self);
        Ok(())
    }

    pub fn has_realtime(&self) -> bool {
        self.streamer.is_some()
    }

    /// Samples per frame for the realtime stream (typically 352).
    pub fn realtime_samples_per_frame(&self) -> Option<i32> {
        self.streamer.as_ref().map(|s| {
            // RealtimeStream does not expose spf publicly; 352 is the SETUP default.
            let _ = s;
            352i32
        })
    }

    /// Encrypt and UDP-write one realtime PCM frame (usually 352×2×2 big-endian
    /// L16). After `setup_realtime` (which already RECORD/FLUSH/syncs), feed
    /// continuously from a capture thread.
    pub fn write_realtime_frame(&mut self, payload: &[u8], marker: bool) -> Result<()> {
        let s = self
            .streamer
            .as_mut()
            .ok_or_else(|| anyhow!("realtime stream not set up"))?;
        s.write_frame(payload, marker)
            .context("write realtime frame")?;
        feedback_if_due(self);
        Ok(())
    }

    pub fn realtime_latency(&self) -> Option<Duration> {
        self.streamer.as_ref().map(|s| s.latency())
    }

    #[cfg(test)]
    pub(crate) fn attach_realtime(&mut self, stream: RealtimeStream) {
        self.streamer = Some(stream);
    }

    /// Send an NTP sync packet (call ~1/s from the live sender).
    pub fn send_realtime_sync(&mut self, first: bool) -> Result<()> {
        let s = self
            .streamer
            .as_mut()
            .ok_or_else(|| anyhow!("realtime stream not set up"))?;
        s.send_sync(first).context("realtime sync")
    }

    pub fn stop(&mut self) -> Result<()> {
        if let Some(b) = self.buffered.as_mut() {
            b.stop();
        }
        if let Some(s) = self.streamer.as_mut() {
            s.stop();
        }
        if let Some(ev) = self.events.as_mut() {
            ev.stop();
        }
        if let Some(t) = self.timing.as_mut() {
            t.stop();
        }
        if let Some(p) = self.ptp.as_ref() {
            p.stop();
        }
        if !self.rtsp_path.is_empty() && self.rtsp.is_some() {
            let path = self.rtsp_path.clone();
            let _ = self.rtsp_mut().do_request(setup::new_teardown(&path));
        }
        self.rtsp = None;
        self.rtsp_path.clear();
        Ok(())
    }
}

// ---- synchronized (stereo-pair) playback -----------------------------------

fn feedback_if_due(c: &mut Client) {
    let _ = feedback_checked(c);
}

fn feedback_checked(c: &mut Client) -> Result<()> {
    if c.last_feedback.elapsed() >= Duration::from_secs(2) {
        c.last_feedback = Instant::now();
        let rtsp = c.rtsp.as_mut().ok_or(crate::session::SessionError::ConnectionLost)?;
        let resp = rtsp.do_request(setup::new_feedback()).context("feedback")?;
        if resp.status_code != 200 {
            return Err(crate::session::SessionError::ReceiverRejected { status: resp.status_code }.into());
        }
    }
    Ok(())
}

impl stream::RealtimeSink for Client {
    fn send_frame(&mut self, payload: &[u8], marker: bool) -> Result<()> {
        let s = self.streamer.as_mut().ok_or(crate::session::SessionError::ConnectionLost)?;
        s.write_frame(payload, marker).context("write realtime frame")
    }
    fn send_sync(&mut self) -> Result<()> {
        self.send_realtime_sync(false)
    }
    fn service(&mut self) -> Result<(u64, u64)> {
        let s = self.streamer.as_mut().ok_or(crate::session::SessionError::ConnectionLost)?;
        let stats = s.service_control()?;
        feedback_checked(self)?;
        Ok(stats)
    }
}

/// Stream realtime PCM to one or more speakers from a single decoded buffer,
/// keeping their RTP timelines locked. With one client this is the plain
/// single-speaker path; with two it drives a stereo pair in lockstep.
pub fn play_pcm_synchronized(clients: &mut [Client], samples: &[f32], sample_rate: i32, channels: i32) -> Result<()> {
    if clients.is_empty() {
        bail!("no clients");
    }
    for c in clients.iter() {
        if c.streamer.is_none() {
            bail!("stream not set up");
        }
    }

    let enc = PcmEncoder::new(sample_rate, channels, 16, false);
    let spf = enc.frame_size();
    let frame_samples = spf * channels as usize;
    if samples.len() < frame_samples {
        bail!("not enough samples for one audio frame");
    }

    // Anchor every speaker to the same seq/timestamp/start-NTP.
    let seq = clients[0].streamer.as_ref().unwrap().seq();
    let rtp_time = clients[0].streamer.as_ref().unwrap().rtp_timestamp();
    let start_ntp = stream::ntp_now();
    for c in clients.iter_mut() {
        let s = c.streamer.as_mut().unwrap();
        s.set_clock(seq, rtp_time, start_ntp);
        let rtp_info = format!("seq={};rtptime={}", s.seq(), s.rtp_timestamp());
        let path = c.rtsp_path.clone();
        let _ = c.rtsp_mut().do_request(setup::new_flush(&path, &rtp_info));
        c.streamer.as_mut().unwrap().send_sync(true).context("first sync")?;
    }

    let interval = clients[0].streamer.as_ref().unwrap().packet_interval();
    let packets_per_sec = (sample_rate / spf as i32).max(1) as usize;
    let mut next = Instant::now();
    for c in clients.iter_mut() {
        c.last_feedback = Instant::now();
    }

    let mut first = true;
    let mut packets = 0usize;
    let mut i = 0;
    while i + frame_samples <= samples.len() {
        let frame = enc.encode(&samples[i..i + frame_samples]);
        for c in clients.iter_mut() {
            c.streamer.as_mut().unwrap().write_frame(&frame, first).context("write frame")?;
        }
        first = false;
        packets += 1;

        if packets % packets_per_sec == 0 {
            for c in clients.iter_mut() {
                if let Err(e) = c.streamer.as_mut().unwrap().send_sync(false) {
                    log::warn!("sync failed: {e}");
                }
            }
        }
        for c in clients.iter_mut() {
            feedback_if_due(c);
        }

        next += interval;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        i += frame_samples;
    }
    Ok(())
}

/// Align realtime RTP clocks across a stereo pair and re-FLUSH/sync so both
/// speakers share one timeline. Call after every client has `setup_realtime`.
pub fn begin_live_realtime_synchronized(clients: &mut [Client]) -> Result<()> {
    if clients.is_empty() {
        bail!("no clients");
    }
    for c in clients.iter() {
        if c.streamer.is_none() {
            bail!("realtime stream not set up");
        }
    }
    let seq = clients[0].streamer.as_ref().unwrap().seq();
    let rtp_time = clients[0].streamer.as_ref().unwrap().rtp_timestamp();
    let start_ntp = stream::ntp_now();
    for c in clients.iter_mut() {
        let s = c.streamer.as_mut().unwrap();
        s.set_clock(seq, rtp_time, start_ntp);
        let rtp_info = format!("seq={};rtptime={}", s.seq(), s.rtp_timestamp());
        let path = c.rtsp_path.clone();
        let _ = c.rtsp_mut().do_request(setup::new_flush(&path, &rtp_info));
        c.streamer
            .as_mut()
            .unwrap()
            .send_sync(true)
            .context("first sync")?;
        c.last_feedback = Instant::now();
    }
    Ok(())
}

/// Arm one or more buffered clients for continuous live send. Aligns RTP clocks
/// to the first client and shares one PTP anchor (stereo / multi-room lockstep).
/// After this returns, call [`Client::write_buffered_frame`] on each client with
/// identical payload bytes (or drop dead clients fail-soft).
pub fn begin_live_buffered_synchronized(clients: &mut [Client]) -> Result<()> {
    arm_buffered_synchronized(clients).map(|_| ())
}

pub(crate) fn arm_buffered_synchronized(clients: &mut [Client]) -> Result<Instant> {
    if clients.is_empty() {
        bail!("no clients");
    }
    for c in clients.iter() {
        if c.buffered.is_none() {
            bail!("buffered stream not set up");
        }
    }

    // ESP multi-room: settle after SETUP before RECORD/SETRATE so tight-sync
    // members join the timing group (PAIR_SETTLE_MS = 1500).
    if clients.len() > 1 {
        let settle = duration_env("AP2_PAIR_SETTLE_MS", 1500);
        log::info!(
            "stereo/multi: settling {:?} before shared SETRATEANCHORTIME (n={})",
            settle,
            clients.len()
        );
        std::thread::sleep(settle);
    }

    let seq = clients[0].buffered.as_ref().unwrap().seq();
    let rtp_time = clients[0].buffered.as_ref().unwrap().rtp_timestamp();
    let shared_anchor = clients[0].ptp_anchor_for_playback();
    let anchor_local = Instant::now() + duration_env("AP2_ANCHOR_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS);
    if shared_anchor.is_none() {
        log::warn!("begin_live_buffered_synchronized: no PTP anchor from leader");
    }

    // Arm all with the *same* seq/rtpTime/anchor (ESP airplay_start_playback loop).
    for c in clients.iter_mut() {
        c.buffered.as_mut().unwrap().set_clock(seq, rtp_time);
        let (cs, cr) = {
            let b = c.buffered.as_ref().unwrap();
            (b.seq(), b.rtp_timestamp())
        };
        c.start_buffered_playback(cs, cr, shared_anchor)
            .with_context(|| {
                format!(
                    "start buffered playback {}",
                    c.device.as_ref().map(|d| d.name.as_str()).unwrap_or("?")
                )
            })?;
        c.last_feedback = Instant::now();
    }
    Ok(anchor_local)
}

impl stream::BufferedSink for Client {
    fn send_frame(&mut self, payload: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        let b = self.buffered.as_mut().ok_or_else(|| anyhow!("buffered stream not set up"))?;
        b.write_frame_cancellable(payload, cancelled).context("write buffered frame")?;
        feedback_checked(self)
    }
}

pub fn stream_buffered_synchronized(
    clients: &mut [Client],
    format: &BufferedAudioFormat,
    feed: &stream::BufferedFeed,
    cancelled: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(stream::BufferedProgress),
) -> Result<stream::BufferedProgress> {
    stream::BufferedCodec::of(format)?;
    let anchor_local = arm_buffered_synchronized(clients).inspect_err(|error| feed.fail(error))?;
    stream_buffered_armed(clients, format, anchor_local, feed, cancelled, on_progress)
}

pub(crate) fn stream_buffered_armed(
    clients: &mut [Client],
    format: &BufferedAudioFormat,
    anchor_local: Instant,
    feed: &stream::BufferedFeed,
    cancelled: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(stream::BufferedProgress),
) -> Result<stream::BufferedProgress> {
    let b = clients.first().and_then(|c| c.buffered.as_ref()).ok_or_else(|| anyhow!("buffered stream not set up"))?;
    let writer = stream::BufferedWriter {
        codec: stream::BufferedCodec::of(format)?,
        samples_per_frame: b.samples_per_frame() as usize,
        interval: b.packet_interval(),
        anchor_local,
        send_lead: duration_env("AP2_SEND_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS),
    };
    let progress = writer.run(clients, feed, cancelled, on_progress)?;
    stream::buffered::wait_until(anchor_local + writer.interval * progress.packets as u32, cancelled)?;
    Ok(progress)
}

/// Stream pre-encoded buffered frames (AAC, 24-bit PCM, or ALAC) to one or
/// more speakers. A single PTP anchor is shared across the pair so the same
/// rtpTime maps to the same network time on every speaker.
pub fn play_buffered_frames_synchronized(clients: &mut [Client], frames: Vec<Vec<u8>>) -> Result<()> {
    if clients.is_empty() {
        bail!("no clients");
    }
    if frames.is_empty() {
        bail!("no frames to play");
    }
    for c in clients.iter() {
        if c.buffered.is_none() {
            bail!("buffered stream not set up");
        }
    }
    let anchor_lead = duration_env("AP2_ANCHOR_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS);
    let send_lead = duration_env("AP2_SEND_LEAD_MS", DEFAULT_ANCHOR_LEAD_MS);

    let seq = clients[0].buffered.as_ref().unwrap().seq();
    let rtp_time = clients[0].buffered.as_ref().unwrap().rtp_timestamp();

    // One anchor for all speakers; anchor_local is the matching instant on our
    // wall clock, used to pace sending below.
    let mut shared_anchor = None;
    let mut anchor_local = Instant::now() + anchor_lead;
    if let Some(ptp) = clients[0].ptp.as_ref() {
        ptp.wait_ready(Duration::from_secs(5));
        if let Some(a) = ptp.anchor_after(anchor_lead) {
            anchor_local = Instant::now() + anchor_lead;
            shared_anchor = Some(a);
        }
    }

    for c in clients.iter_mut() {
        c.buffered.as_mut().unwrap().set_clock(seq, rtp_time);
        let (cs, cr) = {
            let b = c.buffered.as_ref().unwrap();
            (b.seq(), b.rtp_timestamp())
        };
        c.start_buffered_playback(cs, cr, shared_anchor)
            .with_context(|| format!("start buffered playback {}", c.device.as_ref().map(|d| d.name.as_str()).unwrap_or("?")))?;
    }

    let interval = clients[0].buffered.as_ref().unwrap().packet_interval();
    for c in clients.iter_mut() {
        c.last_feedback = Instant::now();
    }

    for (i, frame) in frames.iter().enumerate() {
        let first = i == 0;
        for c in clients.iter_mut() {
            c.buffered.as_mut().unwrap().write_frame(frame, first).with_context(|| format!("write frame {i}"))?;
        }
        for c in clients.iter_mut() {
            feedback_if_due(c);
        }

        // The receiver plays frame i at anchor_local + i*interval; send it
        // send_lead early so its buffer never runs dry.
        let target = anchor_local + interval * (i as u32 + 1);
        if let Some(deadline) = target.checked_sub(send_lead) {
            if let Some(d) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
        }
    }
    // Wait for send_lead after the last frame so the ATV has time to buffer
    // and start playing before the sender sends TEARDOWN. Without this wait
    // the ATV receives TEARDOWN before it renders the first sample.
    if let Some(d) = send_lead.checked_sub(Duration::from_millis(200)) {
        std::thread::sleep(d);
    }
    Ok(())
}

/// Event-channel responder. The receiver issues RTSP keepalive requests on the
/// event port; if we don't answer them with a 200 it stops playing.
struct EventChannel {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    socket: TcpStream,
}

impl EventChannel {
    fn start(stream: TcpStream, shared_secret: &[u8]) -> Result<Self> {
        let cipher = crate::pairing::ChannelCipher::new_event(shared_secret);
        let done = Arc::new(AtomicBool::new(false));
        let done_c = done.clone();
        let socket = stream.try_clone()?;
        let handle = std::thread::spawn(move || event_loop(stream, cipher, done_c));
        Ok(Self {
            done,
            handle: Some(handle),
            socket,
        })
    }

    fn stop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn event_loop(mut stream: TcpStream, mut cipher: crate::pairing::ChannelCipher, done: Arc<AtomicBool>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok();
    let mut inbuf: Vec<u8> = Vec::new();

    let fill = |inbuf: &mut Vec<u8>, stream: &mut TcpStream, cipher: &mut crate::pairing::ChannelCipher| -> bool {
        // Encrypted block reads need a blocking-ish read; on timeout we just
        // retry so the done flag is observed promptly.
        match cipher.read_block(&mut ReadWithRetry { stream, done: &done }) {
            Ok(block) => {
                inbuf.extend_from_slice(&block);
                true
            }
            Err(_) => false,
        }
    };

    log::debug!("event loop started");
    loop {
        if done.load(Ordering::Relaxed) {
            log::debug!("event loop done flag set, returning");
            return;
        }
        // Parse one request: request line + headers (+ optional body).
        let mut cseq = String::new();
        let mut content_len = 0usize;
        let mut got_request_line = false;
        let mut request_line = String::new();
        loop {
            if let Some(idx) = inbuf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&inbuf[..idx]).to_string();
                inbuf.drain(..idx + 2);
                if !got_request_line {
                    got_request_line = true;
                    request_line = line;
                    continue;
                }
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim().to_ascii_lowercase();
                    let v = v.trim();
                    if k == "cseq" {
                        cseq = v.to_string();
                    } else if k == "content-length" {
                        content_len = v.parse().unwrap_or(0);
                    }
                }
            } else if !fill(&mut inbuf, &mut stream, &mut cipher) {
                log::warn!("event loop fill failed, returning");
                return;
            }
        }
        log::debug!("event: {request_line} cseq={cseq} content-length={content_len}");
        while inbuf.len() < content_len {
            if !fill(&mut inbuf, &mut stream, &mut cipher) {
                return;
            }
        }
        let body = inbuf[..content_len].to_vec();
        inbuf.drain(..content_len);

        if !body.is_empty() {
            if let Ok(value) = plist::from_bytes::<plist::Value>(&body) {
                log::debug!("event body: {value:?}");
            } else {
                log::debug!("event body ({} bytes): {}", body.len(), String::from_utf8_lossy(&body));
            }
        }

        let resp = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nServer: AirPlay/960.4.3\r\n\r\n");
        match cipher.encrypt(resp.as_bytes()) {
            Ok(enc) => {
                if stream.write_all(&enc).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Adapts a non-blocking/timeout TcpStream into a blocking reader that honours
/// the channel's done flag (retries on WouldBlock/TimedOut).
struct ReadWithRetry<'a> {
    stream: &'a mut TcpStream,
    done: &'a Arc<AtomicBool>,
}

impl Read for ReadWithRetry<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.done.load(Ordering::Relaxed) {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "event channel stopped"));
            }
            match self.stream.read(buf) {
                Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed")),
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Run the Doubletake `fp-m3` helper binary as a subprocess oracle.
///
/// The helper must read a lower-case hex M2 from stdin and write a lower-case
/// hex M3 to stdout. We validate the returned M3 so a misbehaving helper cannot
/// emit garbage that confuses the Apple TV.
fn compute_fairplay_setup2(helper: &str, m2: &[u8]) -> Result<Vec<u8>> {
    let m2_hex: String = m2.iter().map(|b| format!("{b:02x}")).collect();

    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn FairPlay helper {helper}"))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("missing helper stdin"))?
        .write_all(m2_hex.as_bytes())
        .context("write M2 hex to helper")?;

    let output = child
        .wait_with_output()
        .context("wait for FairPlay helper")?;
    if !output.status.success() {
        bail!(
            "fp-m3 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let m3_hex = String::from_utf8(output.stdout).context("helper stdout is not UTF-8")?;
    let m3 = hex::decode(m3_hex.trim()).context("decode helper M3 hex")?;
    anyhow::ensure!(m3.len() == 164, "unexpected M3 length: {}", m3.len());
    anyhow::ensure!(&m3[..4] == b"FPLY", "M3 is not FPLY framed");
    log::info!("fp-m3 computed setup2 ({} bytes)", m3.len());
    Ok(m3)
}

fn rand_u32() -> u32 {
    rand::thread_rng().next_u32()
}

/// A random locally-administered MAC-format identifier (for the group SETUP
/// deviceID / macAddress).
fn rand_mac() -> String {
    let mut b = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut b);
    b[0] = (b[0] & 0xFE) | 0x02; // locally administered, unicast
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[0], b[1], b[2], b[3], b[4], b[5])
}

/// A random IPv4 Source-Specific Multicast group on the AirPlay group port.
fn rand_group_ipv4() -> String {
    let mut b = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut b);
    format!("232.{}.{}.{}:8738", b[0], b[1], b[2])
}

/// A random IPv6 SSM group on the AirPlay group port.
fn rand_group_ipv6() -> String {
    let mut rng = rand::thread_rng();
    format!("[ff33::{:x}:{:x}]:8738", rng.next_u32() & 0xFFFF, rng.next_u32() & 0xFFFF)
}

fn rand_key32() -> Vec<u8> {
    let mut k = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

fn rand_key32_16() -> Vec<u8> {
    let mut k = vec![0u8; 16];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

/// Build a 72-byte FairPlay `ekey` with the FPLY structure used by Doubletake.
/// The receiver computes the same AES key from this ekey and the M3 we sent
/// during `/fp-setup`.
fn build_fairplay_ekey() -> Vec<u8> {
    let mut ekey = vec![0u8; 72];
    ekey[0..4].copy_from_slice(b"FPLY");
    ekey[4] = 0x01;
    ekey[5] = 0x02;
    ekey[6] = 0x01;
    ekey[7] = 0x00;
    ekey[8] = 0x00;
    ekey[9] = 0x00;
    ekey[10] = 0x00;
    ekey[11] = 0x3c; // remaining bytes = 60
    // chunk1 [16:32] and chunk2 [56:72] are randomized per session
    rand::thread_rng().fill_bytes(&mut ekey[16..32]);
    rand::thread_rng().fill_bytes(&mut ekey[56..72]);
    ekey
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn rand_uuid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// If the Doubletake `fp-m3` helper is present at the repo's `bin/fp-m3`,
    /// exercise the subprocess oracle path against the repository's captured M2
    /// test vector. This is a lightweight integration check: it proves we can
    /// spawn the helper, pass hex, and receive/validate a 164-byte FPLY M3.
    #[test]
    fn fairplay_helper_computes_m3_from_captured_m2() {
        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("bin/fp-m3");
        if !helper.exists() {
            eprintln!("skip: helper not found at {}", helper.display());
            return;
        }

        let m2_hex = "46504c59030102000000008202034a114c26b77d4e2eec2c8f89fdb653b5b32d3576bc176816d110a14c3f53c08dbb936183bfdfe0a4f3c12e85216003b46f738c40c54da6c436d29d1b342d63c7b314309ae79a33bb1787709ef077cbfe4190117a3423e270fd1a2eac44da1a7934f59dc681d1b70783f228c4d077c2d495f5285c3bf8df586fc2ebfe17fb5b65";
        let m2 = hex::decode(m2_hex).expect("valid captured M2");
        let m3 = compute_fairplay_setup2(helper.to_str().unwrap(), &m2).expect("helper run");
        assert_eq!(m3.len(), 164);
        assert_eq!(&m3[..4], b"FPLY");
        assert_eq!(hex::encode(&m3), "46504c590301030000000098038f1a9c991ea22c511e45ba97f1af8dfb0f86f550c54486fe6b3ab233da431ef8e5fc1156dba321fffeabb1b392b09d227e88c712202866eb7bbf310015aa1d19a5df36d5dfd8d3ca1639b376eaece946edfe8b7a66cd302d04aac3c1251714019bd5f2d49b543e11eed1646291ec8efd96b69101b849fd93a02860d1a0dff5cd4414aa4b911e48af23d8406368aeafbb61bfcd569e3e55");
    }

    #[test]
    fn fairplay_helper_rejects_invalid_m3() {
        // Build a fake helper that echoes a too-short M3.
        let tmp = std::env::temp_dir();
        let script = tmp.join("airplay_test_fake_fp_m3.sh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&script, b"#!/bin/sh\ncat >/dev/null\necho '46504c59'")
                .expect("write fake helper");
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            // Skip on non-Unix platforms where we can't easily make an executable script.
            let _ = script;
            return;
        }

        let m2 = vec![0u8; 142];
        let err = compute_fairplay_setup2(script.to_str().unwrap(), &m2).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unexpected M3 length"),
            "expected length error, got: {msg}"
        );
    }
}
