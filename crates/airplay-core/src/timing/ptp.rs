//! PTPv2 (IEEE 1588) software clock for the buffered audio path. AirPlay 2
//! receivers refuse the SETRATEANCHORTIME anchor unless they see us acting as
//! a believable PTP grandmaster on UDP 319/320 (which requires privileged
//! ports — run as root, as the Go/C references do).
//!
//! This is a faithful port of the Go `timing.PTPClock`: it answers/initiates
//! Sync/Follow_Up/Delay_Req exchanges, smooths the measured offset, and hands
//! out an anchor (timeline id + seconds + fraction) at a chosen lead time.

use anyhow::Result;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;

const PTP_MSG_SYNC: u8 = 0x0;
const PTP_MSG_DELAY_REQ: u8 = 0x1;
const PTP_MSG_FOLLOW_UP: u8 = 0x8;
const PTP_MSG_DELAY_RESP: u8 = 0x9;
const PTP_MSG_ANNOUNCE: u8 = 0xB;
const PTP_MSG_SIGNALING: u8 = 0xC;

const PTP_LOCAL_PORT_NUMBER: u16 = 0x8001;

const SMOOTHING_CLAMP_LIMIT: i64 = -2_500_000;
const SMOOTHING_FAST_DIV: i64 = 1;
const SMOOTHING_SLOW_DIV: i64 = 16;
const SMOOTHING_DRIFT_DIV: i64 = 256;
const SMOOTHING_ACQUIRE_NS: u128 = 1_000_000_000;

#[derive(Clone, Copy, Default)]
pub struct PtpAnchor {
    pub timeline_id: u64,
    pub seconds: u64,
    pub fraction: u64,
}

#[derive(Clone, Copy, Default)]
struct SyncRecord {
    seq: u16,
    local_ns: u64,
    consumed: bool,
    populated: bool,
}

struct Smoothing {
    smoothed_offset_ns: i64,
    acq_start: Instant,
}

struct Shared {
    peer_ip: Ipv4Addr,
    clock_id: u64,
    offset_ns: AtomicI64,
    offset_valid: AtomicBool,
    gm_clock_id: AtomicU64,
    gm_valid: AtomicBool,
    delay_resp: AtomicI64,
    done: AtomicBool,
    sync_src: Mutex<Option<SocketAddr>>,
    sync_cache: Mutex<[SyncRecord; 16]>,
    sync_cache_pos: Mutex<usize>,
    smoothing: Mutex<Smoothing>,
}

pub struct PtpClock {
    shared: Arc<Shared>,
    // Held to keep the sockets alive for the clock's lifetime; the worker
    // threads operate on their own Arc clones.
    #[allow(dead_code)]
    event_conn: Arc<UdpSocket>,
    #[allow(dead_code)]
    general_conn: Arc<UdpSocket>,
    // Behind a Mutex so a shared (Arc) clock can be stopped through `&self` —
    // both speakers of a stereo pair hold the same clock.
    handles: Mutex<Vec<JoinHandle<()>>>,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn random_u64() -> u64 {
    rand::RngCore::next_u64(&mut rand::thread_rng())
}

fn ns_to_frac64(ns: u64) -> u64 {
    let hi = ((ns as u128) << 32) / 1_000_000_000;
    let rem = ((ns as u128) << 32) % 1_000_000_000;
    ((hi << 32) | ((rem << 32) / 1_000_000_000)) as u64
}

fn ptp_timestamp_to_ns(b: &[u8]) -> u64 {
    let secs = (b[0] as u64) << 40
        | (b[1] as u64) << 32
        | (b[2] as u64) << 24
        | (b[3] as u64) << 16
        | (b[4] as u64) << 8
        | (b[5] as u64);
    let nanos = u32::from_be_bytes([b[6], b[7], b[8], b[9]]) as u64;
    secs * 1_000_000_000 + nanos
}

fn listen_ptp_udp(port: u16) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&addr.into()).map_err(|e| bind_error(e, port))?;
    let std_sock: UdpSocket = sock.into();
    std_sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(std_sock)
}

pub(crate) fn bind_error(error: std::io::Error, port: u16) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow::Error::new(crate::session::SessionError::PtpPermission)
    } else {
        anyhow::Error::new(error).context(format!("bind PTP UDP :{port}"))
    }
}

impl PtpClock {
    pub fn start(peer_ip: Ipv4Addr) -> Result<Self> {
        Self::with_sockets(peer_ip, listen_ptp_udp(PTP_EVENT_PORT)?, listen_ptp_udp(PTP_GENERAL_PORT)?)
    }

    #[cfg(test)]
    pub(crate) fn locked(timeline_id: u64) -> Self {
        let clock = Self::with_sockets(Ipv4Addr::LOCALHOST, listen_ptp_udp(0).unwrap(), listen_ptp_udp(0).unwrap()).unwrap();
        clock.shared.gm_clock_id.store(timeline_id, Ordering::Relaxed);
        clock.shared.offset_valid.store(true, Ordering::Relaxed);
        clock.shared.gm_valid.store(true, Ordering::Relaxed);
        clock
    }

    #[cfg(test)]
    pub(crate) fn workers(&self) -> usize {
        self.handles.lock().unwrap().len()
    }

    fn with_sockets(peer_ip: Ipv4Addr, event_conn: UdpSocket, general_conn: UdpSocket) -> Result<Self> {
        let event_conn = Arc::new(event_conn);
        let general_conn = Arc::new(general_conn);

        let shared = Arc::new(Shared {
            peer_ip,
            clock_id: random_u64(),
            offset_ns: AtomicI64::new(0),
            offset_valid: AtomicBool::new(false),
            gm_clock_id: AtomicU64::new(0),
            gm_valid: AtomicBool::new(false),
            delay_resp: AtomicI64::new(0),
            done: AtomicBool::new(false),
            sync_src: Mutex::new(None),
            sync_cache: Mutex::new([SyncRecord::default(); 16]),
            sync_cache_pos: Mutex::new(0),
            smoothing: Mutex::new(Smoothing {
                smoothed_offset_ns: 0,
                acq_start: Instant::now(),
            }),
        });

        let mut handles = Vec::new();
        for conn in [event_conn.clone(), general_conn.clone()] {
            let sh = shared.clone();
            let ev = event_conn.clone();
            let ge = general_conn.clone();
            handles.push(std::thread::spawn(move || read_loop(sh, conn, ev, ge)));
        }
        {
            let sh = shared.clone();
            let ev = event_conn.clone();
            let ge = general_conn.clone();
            handles.push(std::thread::spawn(move || send_loop(sh, ev, ge)));
        }

        Ok(Self {
            shared,
            event_conn,
            general_conn,
            handles: Mutex::new(handles),
        })
    }

    pub fn ready(&self) -> bool {
        self.shared.offset_valid.load(Ordering::Relaxed) && self.shared.gm_valid.load(Ordering::Relaxed)
    }

    /// Our announced PTP clock identity, used as `timingPeerInfo.ClockID` in
    /// the entertainment-group SETUP.
    pub fn clock_id(&self) -> u64 {
        self.shared.clock_id
    }

    pub fn delay_responses(&self) -> i64 {
        self.shared.delay_resp.load(Ordering::Relaxed)
    }

    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.ready() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.ready()
    }

    pub fn anchor_after(&self, latency: Duration) -> Option<PtpAnchor> {
        if !self.ready() {
            return None;
        }
        let now = now_ns();
        let offset = self.shared.offset_ns.load(Ordering::Relaxed);
        let lat = latency.as_nanos() as i64;
        let master = now as i64 + offset + lat;
        if master < 0 {
            return None;
        }
        let secs = master as u64 / 1_000_000_000;
        let nanos = master as u64 % 1_000_000_000;
        log::debug!(
            "PTP anchor: now_ns={now} offset={offset} lat={lat} master={master} secs={secs} nanos={nanos}"
        );
        Some(PtpAnchor {
            timeline_id: self.shared.gm_clock_id.load(Ordering::Relaxed),
            seconds: secs,
            fraction: ns_to_frac64(nanos),
        })
    }

    /// Idempotent; safe to call through a shared `Arc` from either speaker.
    pub fn stop(&self) {
        self.shared.done.store(true, Ordering::Relaxed);
        let handles: Vec<JoinHandle<()>> = self.handles.lock().unwrap().drain(..).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for PtpClock {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    fn update_offset(&self, raw_offset: i64) {
        let mut sm = self.smoothing.lock().unwrap();
        if !self.offset_valid.load(Ordering::Relaxed) {
            sm.smoothed_offset_ns = raw_offset;
            sm.acq_start = Instant::now();
            self.offset_ns.store(raw_offset, Ordering::Relaxed);
            self.offset_valid.store(true, Ordering::Relaxed);
            return;
        }
        let mut jitter = raw_offset - sm.smoothed_offset_ns;
        let elapsed = sm.acq_start.elapsed().as_nanos();
        if jitter < 0 {
            if jitter < SMOOTHING_CLAMP_LIMIT {
                jitter = SMOOTHING_CLAMP_LIMIT;
            }
            sm.smoothed_offset_ns += jitter / SMOOTHING_DRIFT_DIV;
        } else if elapsed < SMOOTHING_ACQUIRE_NS {
            sm.smoothed_offset_ns += jitter / SMOOTHING_FAST_DIV;
        } else {
            sm.smoothed_offset_ns += jitter / SMOOTHING_SLOW_DIV;
        }
        self.offset_ns.store(sm.smoothed_offset_ns, Ordering::Relaxed);
    }

    fn record_sync(&self, seq: u16, recv_ns: u64) {
        let mut cache = self.sync_cache.lock().unwrap();
        let mut pos = self.sync_cache_pos.lock().unwrap();
        cache[*pos] = SyncRecord {
            seq,
            local_ns: recv_ns,
            consumed: false,
            populated: true,
        };
        *pos = (*pos + 1) % 16;
    }

    fn consume_sync(&self, seq: u16) -> Option<u64> {
        let mut cache = self.sync_cache.lock().unwrap();
        for rec in cache.iter_mut() {
            if rec.populated && !rec.consumed && rec.seq == seq {
                rec.consumed = true;
                return Some(rec.local_ns);
            }
        }
        None
    }
}

fn read_loop(shared: Arc<Shared>, conn: Arc<UdpSocket>, event_conn: Arc<UdpSocket>, general_conn: Arc<UdpSocket>) {
    let mut buf = [0u8; 512];
    loop {
        if shared.done.load(Ordering::Relaxed) {
            return;
        }
        let (n, from) = match conn.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let from_ip = match from.ip() {
            IpAddr::V4(v4) => v4,
            _ => continue,
        };
        if n < 34 || from_ip != shared.peer_ip {
            continue;
        }
        handle_message(&shared, &buf[..n], from, &event_conn, &general_conn);
    }
}

fn handle_message(
    shared: &Arc<Shared>,
    pkt: &[u8],
    from: SocketAddr,
    event_conn: &UdpSocket,
    general_conn: &UdpSocket,
) {
    let msg_type = pkt[0] & 0x0F;
    let seq = u16::from_be_bytes([pkt[30], pkt[31]]);

    if msg_type == PTP_MSG_ANNOUNCE && pkt.len() >= 64 {
        let gm = u64::from_be_bytes(pkt[53..61].try_into().unwrap());
        if gm != 0 {
            shared.gm_clock_id.store(gm, Ordering::Relaxed);
            shared.gm_valid.store(true, Ordering::Relaxed);
        }
    }

    match msg_type {
        PTP_MSG_SYNC => {
            let local_ns = now_ns();
            shared.record_sync(seq, local_ns);
            {
                let mut src = shared.sync_src.lock().unwrap();
                *src = Some(SocketAddr::new(from.ip(), PTP_EVENT_PORT));
            }
            // One-step clock: origin timestamp is in the Sync message itself.
            if pkt.len() >= 44 {
                let origin_ns = ptp_timestamp_to_ns(&pkt[34..44]);
                if origin_ns > 0 {
                    shared.consume_sync(seq);
                    shared.update_offset(origin_ns as i64 - local_ns as i64);
                }
            }
        }
        PTP_MSG_FOLLOW_UP => {
            if pkt.len() >= 44 {
                let origin_ns = ptp_timestamp_to_ns(&pkt[34..44]);
                if let Some(local_ns) = shared.consume_sync(seq) {
                    shared.update_offset(origin_ns as i64 - local_ns as i64);
                }
            }
        }
        PTP_MSG_DELAY_RESP => {
            shared.delay_resp.fetch_add(1, Ordering::Relaxed);
        }
        PTP_MSG_SIGNALING => {
            send_signaling_interval(shared, general_conn, seq.wrapping_add(1));
        }
        _ => {}
    }
    let _ = event_conn; // event responses are handled by the send loop
}

fn peer_addr(shared: &Shared, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(shared.peer_ip, port))
}

fn fill_common(pkt: &mut [u8], shared: &Shared, msg_type: u8, seq: u16, control: u8, interval: u8) {
    pkt[0] = 0x10 | msg_type;
    pkt[1] = 0x02;
    let len = pkt.len() as u16;
    pkt[2..4].copy_from_slice(&len.to_be_bytes());
    pkt[6] = 0x04;
    pkt[7] = 0x08;
    pkt[20..28].copy_from_slice(&shared.clock_id.to_be_bytes());
    pkt[28..30].copy_from_slice(&PTP_LOCAL_PORT_NUMBER.to_be_bytes());
    pkt[30..32].copy_from_slice(&seq.to_be_bytes());
    pkt[32] = control;
    pkt[33] = interval;
}

fn send_announce(shared: &Shared, general_conn: &UdpSocket, seq: u16) {
    let mut pkt = vec![0u8; 76];
    fill_common(&mut pkt, shared, PTP_MSG_ANNOUNCE, seq, 0x05, 0xFE);
    pkt[44..46].copy_from_slice(&37u16.to_be_bytes());
    pkt[47] = 250;
    pkt[48] = 248;
    pkt[49] = 0x21;
    pkt[50] = 0x43;
    pkt[51] = 0x6A;
    pkt[52] = 240;
    pkt[53..61].copy_from_slice(&shared.clock_id.to_be_bytes());
    pkt[63] = 0xA0;
    pkt[65] = 0x08;
    pkt[67] = 0x08;
    pkt[68..76].copy_from_slice(&shared.clock_id.to_be_bytes());
    let _ = general_conn.send_to(&pkt, peer_addr(shared, PTP_GENERAL_PORT));
}

fn send_sync(shared: &Shared, event_conn: &UdpSocket, seq: u16) {
    let mut pkt = vec![0u8; 44];
    fill_common(&mut pkt, shared, PTP_MSG_SYNC, seq, 0x00, 0xFD);
    pkt[6] = 0x06;
    let _ = event_conn.send_to(&pkt, peer_addr(shared, PTP_EVENT_PORT));
}

fn send_follow_up(shared: &Shared, general_conn: &UdpSocket, seq: u16, origin_ns: u64) {
    let mut pkt = vec![0u8; 96];
    fill_common(&mut pkt, shared, PTP_MSG_FOLLOW_UP, seq, 0x02, 0xFD);
    let secs = origin_ns / 1_000_000_000;
    let nanos = (origin_ns % 1_000_000_000) as u32;
    pkt[34] = (secs >> 40) as u8;
    pkt[35] = (secs >> 32) as u8;
    pkt[36] = (secs >> 24) as u8;
    pkt[37] = (secs >> 16) as u8;
    pkt[38] = (secs >> 8) as u8;
    pkt[39] = secs as u8;
    pkt[40..44].copy_from_slice(&nanos.to_be_bytes());
    pkt[44..54].copy_from_slice(&[0x00, 0x03, 0x00, 0x1c, 0x00, 0x80, 0xc2, 0x00, 0x00, 0x01]);
    pkt[76..86].copy_from_slice(&[0x00, 0x03, 0x00, 0x10, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x04]);
    pkt[86..94].copy_from_slice(&shared.clock_id.to_be_bytes());
    let _ = general_conn.send_to(&pkt, peer_addr(shared, PTP_GENERAL_PORT));
}

fn send_delay_req(shared: &Shared, event_conn: &UdpSocket, seq: u16) {
    let mut pkt = vec![0u8; 44];
    fill_common(&mut pkt, shared, PTP_MSG_DELAY_REQ, seq, 0x00, 0xFD);
    let dest = shared
        .sync_src
        .lock()
        .unwrap()
        .unwrap_or_else(|| peer_addr(shared, PTP_EVENT_PORT));
    let _ = event_conn.send_to(&pkt, dest);
}

fn send_signaling_apple(shared: &Shared, general_conn: &UdpSocket, seq: u16) {
    let tlvs: [u8; 62] = [
        0x00, 0x03, 0x00, 0x16, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x27, 0x10,
        0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x05,
        0x00, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10,
        0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut pkt = vec![0u8; 44 + tlvs.len()];
    fill_common(&mut pkt, shared, PTP_MSG_SIGNALING, seq, 0x05, 0x80);
    pkt[44..].copy_from_slice(&tlvs);
    let _ = general_conn.send_to(&pkt, peer_addr(shared, PTP_GENERAL_PORT));
}

fn send_signaling_interval(shared: &Shared, general_conn: &UdpSocket, seq: u16) {
    let tlv: [u8; 16] = [
        0x00, 0x03, 0x00, 0x0c, 0x00, 0x80, 0xc2, 0x00, 0x00, 0x02, 0x7e, 0x7e, 0x7e, 0x03, 0x00, 0x00,
    ];
    let mut pkt = vec![0u8; 44 + tlv.len()];
    fill_common(&mut pkt, shared, PTP_MSG_SIGNALING, seq, 0x05, 0x80);
    pkt[44..].copy_from_slice(&tlv);
    let _ = general_conn.send_to(&pkt, peer_addr(shared, PTP_GENERAL_PORT));
}

fn send_loop(shared: Arc<Shared>, event_conn: Arc<UdpSocket>, general_conn: Arc<UdpSocket>) {
    let mut signaling_seq: u16 = 0;
    let mut announce_seq: u16 = 1;
    let mut sync_seq: u16 = 1;
    let mut delay_seq: u16 = 0;
    let mut sync_pairs = 0;

    send_signaling_apple(&shared, &general_conn, signaling_seq);
    signaling_seq += 1;
    send_announce(&shared, &general_conn, announce_seq);
    announce_seq += 1;

    let start = Instant::now();
    let mut last_announce = start;
    let mut last_signaling = start;
    let mut last_sync = start;
    let mut last_delay = start;

    loop {
        if shared.done.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
        let now = Instant::now();

        if now.duration_since(last_announce) >= Duration::from_secs(1) {
            last_announce = now;
            if announce_seq <= 3 {
                send_announce(&shared, &general_conn, announce_seq);
                announce_seq += 1;
            }
        }
        if now.duration_since(last_signaling) >= Duration::from_secs(3) {
            last_signaling = now;
            send_signaling_apple(&shared, &general_conn, signaling_seq);
            signaling_seq += 1;
        }
        if now.duration_since(last_sync) >= Duration::from_millis(250) {
            last_sync = now;
            if sync_pairs < 2 {
                let origin = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
                send_sync(&shared, &event_conn, sync_seq);
                send_follow_up(&shared, &general_conn, sync_seq, origin);
                sync_seq += 1;
                sync_pairs += 1;
            }
        }
        if now.duration_since(last_delay) >= Duration::from_millis(250) {
            last_delay = now;
            if shared.offset_valid.load(Ordering::Relaxed) {
                send_delay_req(&shared, &event_conn, delay_seq);
                delay_seq += 1;
            }
        }
    }
}
