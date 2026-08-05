//! Chromecast Cast v2 sender.
//!
//! Ports `apps/p4-bridge/main/chromecast.c`: TLS to port 8009, protobuf
//! CastMessage framing, CONNECT / LAUNCH Default Media Receiver (CC1AD845) /
//! LOAD of the bridge's live HTTP WAV stream, SET_VOLUME, multi-target
//! fail-soft, and teardown on stop.

use crate::control::SpeakerTarget;
use anyhow::{anyhow, bail, Context, Result};
use native_tls::{TlsConnector, TlsStream};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CAST_PORT: u16 = 8009;
const CAST_NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const CAST_NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const CAST_NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const CAST_APP_DEFAULT: &str = "CC1AD845";
const CAST_PROTO_VERSION: u64 = 0;
const CAST_CONNECT_MSG: &str = r#"{"type": "CONNECT", "origin": {}, "userAgent": "VinylBridge","senderInfo": {"sdkType": 2, "version": "15.605.1.3","browserVersion": "44.0.2403.30", "platform": 4,"systemVersion": "vinyl-bridge-pi","connectionType": 1}}"#;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_READ: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize)]
pub struct ChromecastSessionInfo {
    pub name: String,
    pub ip: String,
    pub active: bool,
    pub note: String,
}

struct SessionHandle {
    #[allow(dead_code)]
    name: String,
    ip: String,
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

pub struct ChromecastManager {
    active: AtomicBool,
    sessions: Mutex<Vec<SessionHandle>>,
}

impl ChromecastManager {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            sessions: Mutex::new(Vec::new()),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Start Cast sessions toward the given targets (fail-soft per device).
    ///
    /// `stream_url` must be a LAN-reachable HTTP URL (not 127.0.0.1), e.g.
    /// `http://192.168.1.10/stream.wav`.
    pub fn start(
        &self,
        targets: &[SpeakerTarget],
        stream_url: &str,
        volume_db: f32,
    ) -> Result<()> {
        if targets.is_empty() {
            bail!("no Chromecast targets");
        }
        self.stop();

        let volume = db_to_cast_scalar(volume_db);
        let mut handles = Vec::new();
        let mut ok = 0usize;
        let mut last_err: Option<String> = None;

        for t in targets {
            if !matches!(t.target_type, crate::control::TargetType::Chromecast) {
                continue;
            }
            let port = cast_port_for(t);
            let name = t.name.clone();
            let ip = t.ip.clone();
            let url = stream_url.to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thr = stop.clone();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

            let ip_thr = ip.clone();
            let join = thread::Builder::new()
                .name(format!("cast-{ip}"))
                .spawn(move || {
                    match CastSession::open_and_play(&ip_thr, port, &url, volume) {
                        Ok(mut sess) => {
                            let _ = ready_tx.send(Ok(()));
                            sess.keepalive_until(&stop_thr);
                            sess.teardown();
                        }
                        Err(e) => {
                            tracing::warn!(ip = %ip_thr, error = %e, "chromecast: session failed");
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                })
                .context("spawn cast session thread")?;

            match ready_rx.recv_timeout(Duration::from_secs(30)) {
                Ok(Ok(())) => {
                    tracing::info!(%name, %ip, port, %stream_url, "chromecast: playing");
                    handles.push(SessionHandle {
                        name,
                        ip,
                        stop,
                        join,
                    });
                    ok += 1;
                }
                Ok(Err(e)) => {
                    last_err = Some(format!("{ip}: {e}"));
                    stop.store(true, Ordering::Relaxed);
                    let _ = join.join();
                }
                Err(_) => {
                    last_err = Some(format!("{ip}: timed out starting session"));
                    stop.store(true, Ordering::Relaxed);
                    // Thread may still be mid-handshake; detach join best-effort.
                    let _ = join.join();
                }
            }
        }

        if ok == 0 {
            bail!(
                "no Chromecast reachable{}",
                last_err
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            );
        }

        if ok < targets
            .iter()
            .filter(|t| matches!(t.target_type, crate::control::TargetType::Chromecast))
            .count()
        {
            tracing::warn!(
                ok,
                err = last_err.as_deref().unwrap_or(""),
                "chromecast: partial multi-target start"
            );
        }

        *self.sessions.lock() = handles;
        self.active.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        let mut sessions = self.sessions.lock();
        if sessions.is_empty() {
            self.active.store(false, Ordering::Relaxed);
            return;
        }
        for s in sessions.iter() {
            s.stop.store(true, Ordering::Relaxed);
        }
        let taken = std::mem::take(&mut *sessions);
        drop(sessions);
        for s in taken {
            // Join with a soft bound: teardown is best-effort inside the thread.
            if let Err(e) = s.join.join() {
                tracing::debug!(ip = %s.ip, ?e, "chromecast: session thread join");
            }
        }
        self.active.store(false, Ordering::Relaxed);
        tracing::info!("chromecast: sessions stopped");
    }

    pub fn info(&self, targets: &[SpeakerTarget]) -> Vec<ChromecastSessionInfo> {
        let sessions = self.sessions.lock();
        let active_ips: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.ip.as_str()).collect();
        targets
            .iter()
            .filter(|t| matches!(t.target_type, crate::control::TargetType::Chromecast))
            .map(|t| {
                let active = active_ips.contains(t.ip.as_str()) && self.is_active();
                ChromecastSessionInfo {
                    name: t.name.clone(),
                    ip: t.ip.clone(),
                    active,
                    note: if active {
                        "cast v2 — Default Media Receiver".into()
                    } else if self.is_active() {
                        "not connected (fail-soft skip)".into()
                    } else {
                        "idle".into()
                    },
                }
            })
            .collect()
    }
}

impl Default for ChromecastManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ChromecastManager {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Map dB attenuation to Cast volume scalar (matches ESP `db_to_cast_scalar`):
/// 0 dB → 1.0, −30 dB → 0.0.
pub fn db_to_cast_scalar(db: f32) -> f32 {
    let s = 1.0 + db / 30.0;
    s.clamp(0.0, 1.0)
}

/// Discover a non-loopback LAN IPv4 (UDP connect trick; same idea as mdns_svc).
pub fn discover_lan_ipv4() -> String {
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if let IpAddr::V4(v4) = addr.ip() {
                    if !v4.is_loopback() {
                        return v4.to_string();
                    }
                }
            }
        }
    }
    "127.0.0.1".into()
}

/// Build a Cast-reachable media URL for the live capture stream.
///
/// Prefers main HTTP `/stream.wav` (port 80 omits the port). Falls back to the
/// dedicated capture tap (`:stream_port/capture.wav`) only if `http_port` is 0.
pub fn build_stream_url(http_port: u16, stream_port: u16) -> String {
    let ip = discover_lan_ipv4();
    if http_port == 0 {
        return format!("http://{ip}:{stream_port}/capture.wav");
    }
    if http_port == 80 {
        format!("http://{ip}/stream.wav")
    } else {
        format!("http://{ip}:{http_port}/stream.wav")
    }
}

fn cast_port_for(t: &SpeakerTarget) -> u16 {
    // mDNS sets the real Cast port; manual targets often keep AirPlay default 7000.
    if t.port == 0 || t.port == 7000 {
        CAST_PORT
    } else {
        t.port
    }
}

/* ---------------- session / TLS ---------------- */

struct CastSession {
    ip: String,
    stream: TlsStream<TcpStream>,
    source_id: String,
    dest_id: String,
    session_id: String,
    transport_id: String,
    media_session_id: i32,
    request_id: i32,
    app_launched: bool,
}

impl CastSession {
    fn open_and_play(ip: &str, port: u16, stream_url: &str, volume: f32) -> Result<Self> {
        let mut sess = Self::connect(ip, port)?;
        sess.setup()?;
        sess.set_volume(volume)?;
        sess.load(stream_url)?;
        Ok(sess)
    }

    fn connect(ip: &str, port: u16) -> Result<Self> {
        let addr = format!("{ip}:{port}");
        let tcp = TcpStream::connect_timeout(
            &addr
                .parse()
                .with_context(|| format!("parse cast address {addr}"))?,
            IO_TIMEOUT,
        )
        .with_context(|| format!("TCP connect {addr}"))?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        tcp.set_nodelay(true).ok();

        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .context("tls connector")?;
        let stream = connector
            .connect(ip, tcp)
            .with_context(|| format!("TLS handshake {addr}"))?;

        let mut sess = Self {
            ip: ip.to_string(),
            stream,
            source_id: "sender-0".into(),
            dest_id: "receiver-0".into(),
            session_id: String::new(),
            transport_id: String::new(),
            media_session_id: 0,
            request_id: 1,
            app_launched: false,
        };

        sess.send_msg(
            &sess.source_id.clone(),
            &sess.dest_id.clone(),
            CAST_NS_CONNECTION,
            CAST_CONNECT_MSG,
        )?;
        tracing::info!(%ip, port, "chromecast: TLS connected");
        Ok(sess)
    }

    fn setup(&mut self) -> Result<()> {
        let req_id = self.next_id();
        let req = format!(r#"{{"type":"GET_STATUS","requestId":{req_id}}}"#);
        self.send_msg(
            &self.source_id.clone(),
            &self.dest_id.clone(),
            CAST_NS_RECEIVER,
            &req,
        )?;

        let resp = self
            .wait_json(&["RECEIVER_STATUS"], Duration::from_secs(5))
            .context("GET_STATUS")?;
        self.apply_receiver_status(&resp);

        if !self.app_launched {
            let req_id = self.next_id();
            let req = format!(
                r#"{{"type":"LAUNCH","requestId":{req_id},"appId":"{CAST_APP_DEFAULT}"}}"#
            );
            self.send_msg(
                &self.source_id.clone(),
                &self.dest_id.clone(),
                CAST_NS_RECEIVER,
                &req,
            )?;
            // LAUNCH may reply LAUNCH_STATUS then RECEIVER_STATUS; accept either.
            if let Ok(r) = self.wait_json(
                &["RECEIVER_STATUS", "LAUNCH_STATUS"],
                Duration::from_secs(10),
            ) {
                self.apply_receiver_status(&r);
            }
            // Sometimes the useful status is the next message.
            if self.transport_id.is_empty() {
                if let Ok(r) = self.wait_json(&["RECEIVER_STATUS"], Duration::from_secs(5)) {
                    self.apply_receiver_status(&r);
                }
            }
            self.app_launched = true;
            tracing::info!(ip = %self.ip, "chromecast: launched Default Media Receiver");
        } else {
            tracing::info!(
                ip = %self.ip,
                transport = %self.transport_id,
                "chromecast: Default Media Receiver already running"
            );
        }

        if !self.transport_id.is_empty() && self.transport_id != "receiver-0" {
            self.ensure_channel(&self.transport_id.clone())?;
        }
        Ok(())
    }

    fn apply_receiver_status(&mut self, root: &Value) {
        let status = match root.get("status") {
            Some(s) => s,
            None => return,
        };
        if let Some(sid) = status.get("sessionId").and_then(|v| v.as_str()) {
            self.session_id = sid.to_string();
        }
        if let Some(tid) = status.get("transportId").and_then(|v| v.as_str()) {
            self.transport_id = tid.to_string();
        }
        if let Some(apps) = status.get("applications").and_then(|a| a.as_array()) {
            if let Some(first) = apps.first() {
                let app_id = first.get("appId").and_then(|v| v.as_str()).unwrap_or("");
                if app_id == CAST_APP_DEFAULT {
                    self.app_launched = true;
                    if let Some(tid) = first.get("transportId").and_then(|v| v.as_str()) {
                        self.transport_id = tid.to_string();
                    }
                    if let Some(sid) = first.get("sessionId").and_then(|v| v.as_str()) {
                        self.session_id = sid.to_string();
                    }
                }
            }
        }
    }

    fn ensure_channel(&mut self, dest_id: &str) -> Result<()> {
        self.send_msg(
            &self.source_id.clone(),
            dest_id,
            CAST_NS_CONNECTION,
            CAST_CONNECT_MSG,
        )?;
        tracing::debug!(ip = %self.ip, dest = dest_id, "chromecast: channel opened");
        Ok(())
    }

    fn set_volume(&mut self, mut scalar: f32) -> Result<()> {
        scalar = scalar.clamp(0.0, 1.0);
        let req_id = self.next_id();
        let req = format!(
            r#"{{"type":"SET_VOLUME","requestId":{req_id},"volume":{{"level":{scalar:.2}}}}}"#
        );
        self.send_msg(
            &self.source_id.clone(),
            &self.dest_id.clone(),
            CAST_NS_RECEIVER,
            &req,
        )?;
        Ok(())
    }

    fn load(&mut self, stream_url: &str) -> Result<()> {
        let dest = if !self.transport_id.is_empty() {
            self.transport_id.clone()
        } else {
            self.dest_id.clone()
        };
        let req_id = self.next_id();
        let load = json!({
            "type": "LOAD",
            "requestId": req_id,
            "sessionId": self.session_id,
            "autoplay": true,
            "media": {
                "contentId": stream_url,
                "contentType": "audio/wav",
                "streamType": "BUFFERED"
            }
        });
        let payload = serde_json::to_string(&load)?;
        self.send_msg(
            &self.source_id.clone(),
            &dest,
            CAST_NS_MEDIA,
            &payload,
        )?;

        let resp = self
            .wait_json(
                &["MEDIA_STATUS", "LOAD_FAILED", "INVALID_REQUEST"],
                Duration::from_secs(10),
            )
            .context("LOAD")?;

        let ty = resp.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        if ty == "LOAD_FAILED" || ty == "INVALID_REQUEST" {
            bail!("LOAD failed: {resp}");
        }
        if let Some(status) = resp.get("status") {
            // status may be object or array of media statuses
            if let Some(msid) = status.get("mediaSessionId").and_then(|v| v.as_i64()) {
                self.media_session_id = msid as i32;
            } else if let Some(arr) = status.as_array() {
                if let Some(msid) = arr
                    .first()
                    .and_then(|s| s.get("mediaSessionId"))
                    .and_then(|v| v.as_i64())
                {
                    self.media_session_id = msid as i32;
                }
            }
        }
        tracing::info!(
            ip = %self.ip,
            session = %self.session_id,
            dest = %dest,
            url = %stream_url,
            media_session = self.media_session_id,
            "chromecast: LOAD ok"
        );
        Ok(())
    }

    fn keepalive_until(&mut self, stop: &AtomicBool) {
        let mut last_ping = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            // Opportunistic outbound PING every ~20s (ESP feedback loop).
            if last_ping.elapsed() >= Duration::from_secs(20) {
                let _ = self.send_msg(
                    &self.source_id.clone(),
                    &self.dest_id.clone(),
                    CAST_NS_CONNECTION,
                    r#"{"type":"PING"}"#,
                );
                last_ping = Instant::now();
            }
            match self.try_read_json(KEEPALIVE_READ) {
                Ok(Some(v)) => {
                    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if ty == "PING" {
                        let _ = self.send_msg(
                            &self.source_id.clone(),
                            &self.dest_id.clone(),
                            CAST_NS_CONNECTION,
                            r#"{"type":"PONG"}"#,
                        );
                    } else if ty == "CLOSE" {
                        tracing::warn!(ip = %self.ip, "chromecast: receiver closed channel");
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(ip = %self.ip, error = %e, "chromecast: keepalive I/O error");
                    break;
                }
            }
        }
    }

    fn teardown(&mut self) {
        if self.media_session_id > 0 {
            let req_id = self.next_id();
            let dest = if !self.transport_id.is_empty() {
                self.transport_id.clone()
            } else {
                self.dest_id.clone()
            };
            let stop_req = format!(
                r#"{{"type":"STOP","requestId":{req_id},"mediaSessionId":{}}}"#,
                self.media_session_id
            );
            let _ = self.send_msg(
                &self.source_id.clone(),
                &dest,
                CAST_NS_MEDIA,
                &stop_req,
            );
            thread::sleep(Duration::from_millis(200));
        }
        let _ = self.send_msg(
            &self.source_id.clone(),
            &self.dest_id.clone(),
            CAST_NS_CONNECTION,
            r#"{"type":"CLOSE"}"#,
        );
        thread::sleep(Duration::from_millis(100));
        tracing::info!(ip = %self.ip, "chromecast: session torn down");
    }

    fn next_id(&mut self) -> i32 {
        let id = self.request_id;
        self.request_id += 1;
        id
    }

    fn send_msg(&mut self, source: &str, dest: &str, ns: &str, json_payload: &str) -> Result<()> {
        let pb = encode_cast_message(source, dest, ns, json_payload);
        let mut wire = Vec::with_capacity(4 + pb.len());
        let len = pb.len() as u32;
        wire.extend_from_slice(&len.to_be_bytes());
        wire.extend_from_slice(&pb);
        self.stream
            .write_all(&wire)
            .with_context(|| format!("cast write to {}", self.ip))?;
        Ok(())
    }

    fn wait_json(&mut self, types: &[&str], timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(Duration::from_secs(2));
            match self.try_read_json(wait)? {
                Some(v) => {
                    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if ty == "PING" {
                        let _ = self.send_msg(
                            &self.source_id.clone(),
                            &self.dest_id.clone(),
                            CAST_NS_CONNECTION,
                            r#"{"type":"PONG"}"#,
                        );
                        continue;
                    }
                    if types.iter().any(|t| *t == ty) {
                        return Ok(v);
                    }
                    tracing::debug!(ip = %self.ip, ty, "chromecast: ignoring message");
                }
                None => {}
            }
        }
        Err(anyhow!(
            "timeout waiting for {} from {}",
            types.join("|"),
            self.ip
        ))
    }

    /// Read one Cast message. `Ok(None)` = read timeout (no data).
    fn try_read_json(&mut self, timeout: Duration) -> Result<Option<Value>> {
        self.stream
            .get_ref()
            .set_read_timeout(Some(timeout))
            .ok();

        let mut hdr = [0u8; 4];
        match read_exact_timeout(&mut self.stream, &mut hdr) {
            Ok(()) => {}
            Err(e) if is_timeout(&e) => return Ok(None),
            Err(e) => return Err(e).context("read cast header"),
        }
        let msg_len = u32::from_be_bytes(hdr) as usize;
        if msg_len == 0 || msg_len > 65536 {
            bail!("invalid cast message length {msg_len}");
        }
        let mut msg = vec![0u8; msg_len];
        read_exact_timeout(&mut self.stream, &mut msg).context("read cast body")?;
        let payload = decode_cast_payload(&msg).context("decode cast payload")?;
        let v: Value = serde_json::from_str(&payload)
            .with_context(|| format!("parse cast json: {payload}"))?;
        Ok(Some(v))
    }
}

fn read_exact_timeout(stream: &mut TlsStream<TcpStream>, buf: &mut [u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        match stream.read(&mut buf[off..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "cast connection closed",
                ))
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_timeout(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut
}

/* ---------------- protobuf CastMessage ---------------- */

fn pb_varint(buf: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        buf.push((val as u8) | 0x80);
        val >>= 7;
    }
    buf.push(val as u8);
}

fn pb_string(buf: &mut Vec<u8>, field_num: u32, s: &str) {
    let tag = (field_num << 3) | 2;
    pb_varint(buf, tag as u64);
    pb_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

fn pb_varint_field(buf: &mut Vec<u8>, field_num: u32, val: u64) {
    let tag = (field_num << 3) | 0;
    pb_varint(buf, tag as u64);
    pb_varint(buf, val);
}

fn encode_cast_message(source_id: &str, dest_id: &str, ns: &str, json_payload: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256 + json_payload.len());
    // Field 1: protocol_version = CASTV2_1_0 = 0
    pb_varint_field(&mut buf, 1, CAST_PROTO_VERSION);
    pb_string(&mut buf, 2, source_id);
    pb_string(&mut buf, 3, dest_id);
    pb_string(&mut buf, 4, ns);
    pb_varint_field(&mut buf, 5, 0); // payload_type STRING = 0
    pb_string(&mut buf, 6, json_payload);
    buf
}

fn decode_cast_payload(data: &[u8]) -> Result<String> {
    let mut pos = 0usize;
    while pos < data.len() {
        let (tag, np) = read_varint(data, pos)?;
        pos = np;
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let (_, np) = read_varint(data, pos)?;
                pos = np;
            }
            2 => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                let len = len as usize;
                if pos + len > data.len() {
                    bail!("protobuf length overrun");
                }
                if field_num == 6 && len > 0 {
                    let s = String::from_utf8_lossy(&data[pos..pos + len]).into_owned();
                    return Ok(s);
                }
                pos += len;
            }
            _ => bail!("unsupported protobuf wire type {wire_type}"),
        }
    }
    bail!("cast message missing payload field 6")
}

fn read_varint(data: &[u8], mut pos: usize) -> Result<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0;
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((val, pos));
        }
        shift += 7;
        if shift > 63 {
            bail!("varint too long");
        }
    }
    bail!("truncated varint")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_map() {
        assert!((db_to_cast_scalar(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_cast_scalar(-30.0) - 0.0).abs() < 1e-6);
        assert!((db_to_cast_scalar(-15.0) - 0.5).abs() < 1e-6);
        assert!((db_to_cast_scalar(-100.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn pb_roundtrip_payload() {
        let pb = encode_cast_message("sender-0", "receiver-0", CAST_NS_RECEIVER, r#"{"type":"PING"}"#);
        let payload = decode_cast_payload(&pb).unwrap();
        assert!(payload.contains("PING"));
    }

    #[test]
    fn stream_url_port80() {
        // discover may return loopback in sandbox; still check formatting path.
        let u = build_stream_url(80, 8088);
        assert!(u.ends_with("/stream.wav"), "{u}");
        assert!(!u.contains(":80/"), "{u}");
    }

    #[test]
    fn stream_url_non80() {
        let u = build_stream_url(8080, 8088);
        assert!(u.contains(":8080/stream.wav"), "{u}");
    }
}
