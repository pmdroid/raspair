//! SETUP / RECORD / FLUSH / SETRATEANCHORTIME body builders and response
//! parsers. The plist field names and the realtime-vs-buffered latency rules
//! are taken from the Go reference, which were proven audible against Apple
//! receivers (and against the ESP32 firmware for the buffered anchor lead).

use super::request::*;
use anyhow::{anyhow, bail, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

// SETUP "audioFormat" bitmask values. The bit index doubles as the
// buffered-stream codec index carried in the SSRC high byte.
pub const AUDIO_FORMAT_PCM_44100_16_2: u64 = 0x800;
pub const AUDIO_FORMAT_PCM_48000_16_2: u64 = 0x8000;
pub const AUDIO_FORMAT_PCM_48000_24_2: u64 = 0x20000;
pub const AUDIO_FORMAT_ALAC_44100_16_2: u64 = 0x40000;
pub const AUDIO_FORMAT_ALAC_48000_24_2: u64 = 0x200000;
pub const AUDIO_FORMAT_AAC_44100_2: u64 = 0x400000;
pub const AUDIO_FORMAT_AAC_48000_2: u64 = 0x800000;

#[derive(Serialize)]
struct SetupBaseBody {
    #[serde(rename = "deviceID")]
    device_id: String,
    #[serde(rename = "macAddress")]
    mac_address: String,
    name: String,
    #[serde(rename = "sessionUUID")]
    session_uuid: String,
    #[serde(rename = "groupContainsGroupLeader")]
    group_contains_group_leader: bool,
    #[serde(rename = "isMultiSelectAirPlay")]
    is_multi_select_airplay: bool,
    model: String,
    #[serde(rename = "osName")]
    os_name: String,
    #[serde(rename = "osVersion")]
    os_version: String,
    #[serde(rename = "osBuildVersion")]
    os_build_version: String,
    #[serde(rename = "sourceVersion")]
    source_version: String,
    #[serde(rename = "groupUUID", skip_serializing_if = "Option::is_none")]
    group_uuid: Option<String>,
    #[serde(rename = "senderSupportsRelay")]
    sender_supports_relay: bool,
    #[serde(rename = "statsCollectionEnabled")]
    stats_collection_enabled: bool,
    #[serde(rename = "timingProtocol")]
    timing_protocol: String,
    #[serde(rename = "timingPort", skip_serializing_if = "Option::is_none")]
    timing_port: Option<i64>,
    #[serde(rename = "ekey", with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    ekey: Option<Vec<u8>>,
    #[serde(rename = "eiv", with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    eiv: Option<Vec<u8>>,
    #[serde(rename = "et", skip_serializing_if = "Option::is_none")]
    et: Option<u64>,
    #[serde(rename = "timingPeerInfo", skip_serializing_if = "Option::is_none")]
    timing_peer_info: Option<TimingPeerInfo>,
    #[serde(rename = "timingPeerList", skip_serializing_if = "Option::is_none")]
    timing_peer_list: Option<Vec<TimingPeerInfo>>,
}

#[derive(Serialize, Clone)]
struct TimingPeerInfo {
    #[serde(rename = "Addresses")]
    addresses: Vec<String>,
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "SupportsClockPortMatchingOverride")]
    supports_clock_port_matching_override: bool,
}

#[derive(Serialize)]
struct SetupStreamsBody {
    streams: Vec<SetupStream>,
}

#[derive(Serialize)]
struct SetupStream {
    #[serde(rename = "type")]
    typ: i64,
    #[serde(rename = "audioMode")]
    audio_mode: String,
    #[serde(rename = "audioFormat")]
    audio_format: u64,
    ct: i64,
    #[serde(rename = "isMedia")]
    is_media: bool,
    #[serde(with = "serde_bytes")]
    shk: Vec<u8>,
    #[serde(with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    asc: Option<Vec<u8>>,
    spf: i64,
    sr: i64,
    #[serde(rename = "supportsDynamicStreamID")]
    supports_dynamic_stream_id: bool,
    #[serde(rename = "streamConnectionID")]
    stream_connection_id: u64,
    #[serde(rename = "audioFormatIndex", skip_serializing_if = "Option::is_none")]
    audio_format_index: Option<i64>,
    #[serde(rename = "controlPort")]
    control_port: i64,
    #[serde(rename = "latencyMin")]
    latency_min: i64,
    #[serde(rename = "latencyMax")]
    latency_max: i64,
    #[serde(with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    aeskey: Option<Vec<u8>>,
    #[serde(with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    aesiv: Option<Vec<u8>>,
}

#[derive(Serialize)]
struct RecordBufferedBody {
    streams: Vec<RecordBufferedStream>,
}

#[derive(Serialize)]
struct RecordBufferedStream {
    #[serde(rename = "streamID")]
    stream_id: i64,
    seq: u32,
    #[serde(rename = "rtpTime")]
    rtp_time: u32,
    rate: f64,
}

#[derive(Serialize)]
struct SetRateAnchorTimeBody {
    rate: u64,
    #[serde(rename = "rtpTime")]
    rtp_time: u32,
    #[serde(rename = "networkTimeFlags")]
    network_time_flags: u64,
    #[serde(rename = "networkTimeTimelineID")]
    network_time_timeline_id: u64,
    #[serde(rename = "networkTimeSecs")]
    network_time_secs: u64,
    #[serde(rename = "networkTimeFrac")]
    network_time_frac: u64,
}

#[derive(Serialize)]
struct FlushBufferedBody {
    #[serde(rename = "flushFromSeq")]
    flush_from_seq: u32,
    #[serde(rename = "flushFromTS")]
    flush_from_ts: u32,
    #[serde(rename = "flushUntilSeq")]
    flush_until_seq: u32,
    #[serde(rename = "flushUntilTS")]
    flush_until_ts: u32,
}

#[derive(Deserialize, Default)]
pub struct SetupBaseResponse {
    #[serde(rename = "eventPort", default)]
    pub event_port: i64,
    #[serde(rename = "timingPort", default)]
    pub timing_port: i64,
}

#[derive(Deserialize, Default)]
pub struct SetupResponse {
    #[serde(default)]
    pub streams: Vec<SetupResponseStream>,
}

#[derive(Deserialize, Default, Clone)]
pub struct SetupResponseStream {
    #[serde(rename = "type", default)]
    pub typ: i64,
    #[serde(rename = "streamID", default)]
    pub stream_id: i64,
    #[serde(rename = "dataPort", default)]
    pub data_port: i64,
    #[serde(rename = "controlPort", default)]
    pub control_port: i64,
}

#[derive(Deserialize, Default)]
struct StatusBody {
    #[serde(default)]
    status: i64,
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn marshal<T: Serialize>(body: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, body)?;
    Ok(buf)
}

/// Build the first SETUP, mirroring pyatv's `_setup_base()`. NTP uses an
/// "older generation" iPhone identity; PTP switches to a MacBook identity with
/// ekey/eiv/et and a timing peer list.
pub fn new_base_setup(
    url: &str,
    mac: &str,
    session_uuid: &str,
    timing_protocol: &str,
    timing_port: i64,
    local_addresses: &[String],
) -> Result<Request> {
    let mut body = SetupBaseBody {
        device_id: mac.to_string(),
        mac_address: mac.to_string(),
        name: "ap2-sender".to_string(),
        session_uuid: session_uuid.to_string(),
        group_contains_group_leader: false,
        is_multi_select_airplay: true,
        model: "iPhone14,3".to_string(),
        os_name: "iPhone OS".to_string(),
        os_version: "16.5".to_string(),
        os_build_version: "20F66".to_string(),
        source_version: "690.7.1".to_string(),
        group_uuid: None,
        sender_supports_relay: false,
        stats_collection_enabled: false,
        timing_protocol: timing_protocol.to_string(),
        timing_port: if timing_port > 0 { Some(timing_port) } else { None },
        ekey: None,
        eiv: None,
        et: None,
        timing_peer_info: None,
        timing_peer_list: None,
    };

    if timing_protocol == "PTP" {
        body.model = "MacBookPro18,3".to_string();
        body.os_name = "Mac OS X".to_string();
        body.os_version = "15.5".to_string();
        body.os_build_version = "24F74".to_string();
        body.source_version = "960.4.3".to_string();
        body.group_uuid = Some(session_uuid.to_string());
        body.sender_supports_relay = true;
        body.timing_port = None;
        body.et = Some(0);
        body.ekey = Some(random_bytes(16));
        body.eiv = Some(random_bytes(16));
        let peer = TimingPeerInfo {
            addresses: local_addresses.to_vec(),
            id: session_uuid.to_string(),
            supports_clock_port_matching_override: true,
        };
        body.timing_peer_info = Some(peer.clone());
        body.timing_peer_list = Some(vec![peer]);
    }

    let mut req = Request::new(Method::Setup, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

/// Build the per-stream SETUP. `audio_format_index` is implicit in the format
/// bitmask; latencies are derived from the stream type (0.25s/2s realtime,
/// 0.5s/5s buffered — the long buffered max gives a stereo follower headroom).
#[allow(clippy::too_many_arguments)]
pub fn new_stream_setup(
    url: &str,
    shk: &[u8],
    asc: Option<&[u8]>,
    stream_type: i64,
    spf: i64,
    sample_rate: i64,
    local_control_port: i64,
    audio_format: u64,
    ct: i64,
    stream_connection_id: u64,
    aeskey: Option<Vec<u8>>,
    aesiv: Option<Vec<u8>>,
) -> Result<Request> {
    let mut supports_dynamic_stream_id = false;
    let mut latency_min = sample_rate / 4;
    let mut latency_max = sample_rate * 2;
    if stream_type == 103 {
        supports_dynamic_stream_id = true;
        latency_min = sample_rate / 2;
        latency_max = sample_rate * 5;
    }

    let audio_format_index = if audio_format.count_ones() == 1 {
        Some(audio_format.trailing_zeros() as i64)
    } else {
        None
    };

    let body = SetupStreamsBody {
        streams: vec![SetupStream {
            typ: stream_type,
            audio_mode: "default".to_string(),
            audio_format,
            ct,
            is_media: true,
            shk: shk.to_vec(),
            asc: asc.map(|a| a.to_vec()),
            spf,
            sr: sample_rate,
            supports_dynamic_stream_id,
            stream_connection_id,
            audio_format_index,
            control_port: local_control_port,
            latency_min,
            latency_max,
            aeskey,
            aesiv,
        }],
    };

    let mut req = Request::new(Method::Setup, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

// ---- entertainment / home-theater group session SETUP ---------------------
//
// Field set reverse-engineered from a decrypted macOS sender streaming into an
// Apple-TV-led home-theater group (see docs/ENTERTAINMENT_GROUPS.md). The
// session SETUP carries the multicast group descriptor + a hijackID; the
// receiver fans the audio out to its bound members. On a LAN where multicast
// doesn't carry, the audio falls back to unicast to the leader.

#[derive(Serialize, Clone)]
struct GroupTimingPeer {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Addresses")]
    addresses: Vec<String>,
    #[serde(rename = "ClockID")]
    clock_id: i64,
    #[serde(rename = "DeviceType")]
    device_type: i64,
    #[serde(rename = "SupportsClockPortMatchingOverride")]
    supports_clock_port_matching_override: bool,
}

#[derive(Serialize)]
struct Mc2ucGroupInfo {
    #[serde(rename = "groupIPv4Addr")]
    group_ipv4_addr: String,
    #[serde(rename = "groupIPv6Addr", skip_serializing_if = "Option::is_none")]
    group_ipv6_addr: Option<String>,
    #[serde(rename = "sourceIPv4Addr")]
    source_ipv4_addr: String,
    #[serde(rename = "sourceIPv6Addr", skip_serializing_if = "Option::is_none")]
    source_ipv6_addr: Option<String>,
    #[serde(rename = "groupEncryptionKey", with = "serde_bytes")]
    group_encryption_key: Vec<u8>,
}

#[derive(Serialize)]
struct GroupSessionSetup {
    #[serde(rename = "deviceID")]
    device_id: String,
    #[serde(rename = "macAddress")]
    mac_address: String,
    name: String,
    model: String,
    #[serde(rename = "osName")]
    os_name: String,
    #[serde(rename = "osVersion")]
    os_version: String,
    #[serde(rename = "osBuildVersion")]
    os_build_version: String,
    #[serde(rename = "sourceVersion")]
    source_version: String,
    #[serde(rename = "timingProtocol")]
    timing_protocol: String,
    #[serde(rename = "timingPeerInfo")]
    timing_peer_info: GroupTimingPeer,
    #[serde(rename = "timingPeerList")]
    timing_peer_list: Vec<GroupTimingPeer>,
    #[serde(rename = "groupUUID")]
    group_uuid: String,
    #[serde(rename = "sessionUUID")]
    session_uuid: String,
    #[serde(rename = "sessionCorrelationUUID")]
    session_correlation_uuid: String,
    #[serde(rename = "hijackID")]
    hijack_id: String,
    #[serde(rename = "senderPerceivedClusterType")]
    sender_perceived_cluster_type: i64,
    #[serde(rename = "isMultiSelectAirPlay")]
    is_multi_select_airplay: bool,
    #[serde(rename = "senderSupportsRelay")]
    sender_supports_relay: bool,
    #[serde(rename = "groupContainsGroupLeader")]
    group_contains_group_leader: bool,
    #[serde(rename = "statsCollectionEnabled")]
    stats_collection_enabled: bool,
    #[serde(rename = "updateSessionRequest")]
    update_session_request: bool,
    #[serde(rename = "mc2ucDetectionSSMGroupInfo")]
    mc2uc_detection_ssm_group_info: Mc2ucGroupInfo,
    #[serde(rename = "et", skip_serializing_if = "Option::is_none")]
    et: Option<u64>,
    #[serde(rename = "ekey", with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    ekey: Option<Vec<u8>>,
    #[serde(rename = "eiv", with = "serde_bytes", skip_serializing_if = "Option::is_none")]
    eiv: Option<Vec<u8>>,
}

/// Parameters for the entertainment-group session SETUP. The client fills these
/// (random group multicast addr + key, hijackID, the PTP clock id, ...).
pub struct GroupSetupParams {
    pub device_id: String,
    pub mac_address: String,
    pub name: String,
    pub local_addresses: Vec<String>,
    pub clock_id: i64,
    pub group_uuid: String,
    pub session_uuid: String,
    pub session_correlation_uuid: String,
    pub hijack_id: String,
    pub source_ipv4: String,
    pub group_ipv4: String,
    pub group_ipv6: Option<String>,
    pub source_ipv6: Option<String>,
    pub group_encryption_key: Vec<u8>,
    pub et: Option<u64>,
    pub ekey: Option<Vec<u8>>,
    pub eiv: Option<Vec<u8>>,
}

pub fn new_group_session_setup(url: &str, p: &GroupSetupParams) -> Result<Request> {
    let peer = GroupTimingPeer {
        id: p.session_correlation_uuid.clone(),
        addresses: p.local_addresses.clone(),
        clock_id: p.clock_id,
        device_type: 0,
        supports_clock_port_matching_override: true,
    };
    let body = GroupSessionSetup {
        device_id: p.device_id.clone(),
        mac_address: p.mac_address.clone(),
        name: p.name.clone(),
        // Mimic a current macOS sender (matches the captured session identity).
        model: "Mac17,2".to_string(),
        os_name: "macOS".to_string(),
        os_version: "26.3.1".to_string(),
        os_build_version: "25D2128".to_string(),
        source_version: "935.7.1".to_string(),
        timing_protocol: "PTP".to_string(),
        timing_peer_info: peer.clone(),
        timing_peer_list: vec![peer],
        group_uuid: p.group_uuid.clone(),
        session_uuid: p.session_uuid.clone(),
        session_correlation_uuid: p.session_correlation_uuid.clone(),
        hijack_id: p.hijack_id.clone(),
        sender_perceived_cluster_type: 2,
        is_multi_select_airplay: true,
        sender_supports_relay: true,
        group_contains_group_leader: std::env::var("AP2_GROUP_GCGL")
            .unwrap_or_default()
            == "1",
        stats_collection_enabled: false,
        update_session_request: false,
        mc2uc_detection_ssm_group_info: Mc2ucGroupInfo {
            group_ipv4_addr: p.group_ipv4.clone(),
            group_ipv6_addr: p.group_ipv6.clone(),
            source_ipv4_addr: p.source_ipv4.clone(),
            source_ipv6_addr: p.source_ipv6.clone(),
            group_encryption_key: p.group_encryption_key.clone(),
        },
        et: p.et,
        ekey: p.ekey.clone(),
        eiv: p.eiv.clone(),
    };
    let mut req = Request::new(Method::Setup, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

pub fn new_record(url: &str) -> Request {
    Request::new(Method::Record, url)
}

pub fn new_record_buffered(url: &str, stream_id: i64, seq: u32, rtp_time: u32) -> Result<Request> {
    let body = RecordBufferedBody {
        streams: vec![RecordBufferedStream {
            stream_id,
            seq,
            rtp_time,
            rate: 1.0,
        }],
    };
    let mut req = Request::new(Method::Record, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

pub fn new_flush(url: &str, rtp_info: &str) -> Request {
    let mut req = Request::new(Method::Flush, url);
    req.set_header("Range", "npt=0-");
    if !rtp_info.is_empty() {
        req.set_header(HEADER_RTP_INFO, rtp_info);
    }
    req
}

pub fn new_flush_buffered(
    url: &str,
    flush_from_seq: u32,
    flush_from_ts: u32,
    flush_until_seq: u32,
    flush_until_ts: u32,
) -> Result<Request> {
    let body = FlushBufferedBody {
        flush_from_seq,
        flush_from_ts,
        flush_until_seq,
        flush_until_ts,
    };
    let mut req = Request::new(Method::FlushBuffered, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

/// SETPEERS: a binary plist whose root is an array of IP strings (receiver
/// first, then the sender's local address).
pub fn new_set_peers(url: &str, peers: &[String]) -> Result<Request> {
    let mut req = Request::new(Method::SetPeers, url);
    req.set_body(marshal(&peers.to_vec())?);
    Ok(req)
}

pub fn new_set_rate_anchor_time(
    url: &str,
    rtp_time: u32,
    timeline_id: u64,
    network_secs: u64,
    network_frac: u64,
) -> Result<Request> {
    let body = SetRateAnchorTimeBody {
        rate: 1,
        rtp_time,
        network_time_flags: 0,
        network_time_timeline_id: timeline_id,
        network_time_secs: network_secs,
        network_time_frac: network_frac,
    };
    let mut req = Request::new(Method::SetRateAnchorTime, url);
    req.set_body(marshal(&body)?);
    Ok(req)
}

pub fn new_teardown(url: &str) -> Request {
    Request::new(Method::Teardown, url)
}

pub fn new_set_parameter(url: &str, param: &str, value: &str) -> Request {
    let mut req = Request::new(Method::SetParameter, url);
    req.body = Some(format!("{param}: {value}\r\n").into_bytes());
    req.set_content_type(CONTENT_TYPE_PARAMETERS);
    req
}

pub fn new_feedback() -> Request {
    Request::new(Method::Post, "/feedback")
}

// FairPlay (`/fp-setup`) two-message SAP handshake, captured verbatim from a
// real macOS sender → Apple TV session (atvproxy MITM). An Apple TV that leads
// an entertainment group requires the sender to complete this attestation
// before it will render audio; HomePods don't. The receiver's setup2 reply is
// just `fp_header || last-20-bytes-of-setup2`, so it is largely a structural
// handshake — and since group audio is keyed by the `groupEncryptionKey`
// (no FairPlay `ekey` in the SETUP), the derived FairPlay key is never
// exercised, so replaying the captured blobs is enough to clear the gate.
pub const FP_SETUP1: &[u8] = &[
    0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x03, 0xbb,
];

pub const FP_SETUP2: &[u8] = &[
    0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x98, 0x03, 0x8f, 0x1a, 0x9c,
    0x9a, 0x53, 0xee, 0x1b, 0x2b, 0x87, 0x24, 0x54, 0xb4, 0xf5, 0x42, 0xfd, 0xd7, 0xf9, 0xe5, 0x12,
    0x44, 0xf2, 0x7e, 0x8c, 0xf9, 0x38, 0x3d, 0x1c, 0x32, 0x6e, 0xbf, 0xd0, 0x6e, 0xab, 0x0f, 0x58,
    0xa0, 0x23, 0x20, 0x2c, 0x70, 0x74, 0xca, 0x8e, 0x4d, 0x3b, 0xde, 0xa4, 0xa6, 0x49, 0x1d, 0x35,
    0xf3, 0x7c, 0x66, 0xa9, 0xfc, 0x4f, 0x35, 0x23, 0x60, 0x17, 0x8e, 0x35, 0xdf, 0xe6, 0x6c, 0x09,
    0x52, 0x3b, 0xec, 0xd7, 0x59, 0xd2, 0x53, 0x3d, 0xff, 0xc8, 0x3b, 0x78, 0xf9, 0x32, 0x0a, 0x68,
    0x60, 0x03, 0x12, 0x18, 0xee, 0x99, 0xa2, 0x5d, 0x25, 0x81, 0xda, 0x6a, 0xaa, 0x33, 0x26, 0xf5,
    0x59, 0x19, 0x7f, 0x42, 0xd0, 0xc0, 0x35, 0xd3, 0x29, 0x74, 0x09, 0x9f, 0x29, 0x2a, 0x77, 0x30,
    0x04, 0xc4, 0xce, 0x15, 0xcb, 0x7a, 0x31, 0xe5, 0x6d, 0xd9, 0x5a, 0xd9, 0x15, 0xeb, 0xd9, 0x4a,
    0x93, 0x47, 0x48, 0x0b, 0xd2, 0x99, 0xc6, 0x23, 0x97, 0xb8, 0x6e, 0xbf, 0x43, 0xfd, 0x03, 0x38,
    0x63, 0x82, 0x78, 0x67,
];

/// Build a `POST /fp-setup` carrying one of the FairPlay handshake messages.
pub fn new_fp_setup(body: &[u8]) -> Request {
    let mut req = Request::new(Method::Post, "/fp-setup");
    req.set_header("X-Apple-ET", "32");
    req.body = Some(body.to_vec());
    req.set_content_type(CONTENT_TYPE_OCTET_STREAM);
    req
}

pub fn parse_setup_base_response(resp: &Response) -> Result<SetupBaseResponse> {
    if resp.body.is_empty() {
        bail!("empty response body");
    }
    Ok(plist::from_bytes(&resp.body)?)
}

pub fn parse_setup_response(resp: &Response) -> Result<SetupResponse> {
    if resp.body.is_empty() {
        bail!("empty response body");
    }
    Ok(plist::from_bytes(&resp.body)?)
}

/// Returns (status, present). An empty body means "no status reported".
pub fn parse_status(resp: &Response) -> Result<(i64, bool)> {
    if resp.body.is_empty() {
        return Ok((0, false));
    }
    let body: StatusBody = plist::from_bytes(&resp.body)?;
    Ok((body.status, true))
}

pub fn find_stream(resp: &SetupResponse, stream_type: i64) -> Result<SetupResponseStream> {
    resp.streams
        .iter()
        .find(|s| s.typ == stream_type)
        .cloned()
        .ok_or_else(|| anyhow!("stream type {stream_type} not found in response"))
}

pub fn addr_to_rtsp_url(addr: &str, session_uuid: &str) -> String {
    format!("rtsp://{addr}/{session_uuid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_setup_serializes_to_binary_plist() {
        // PTP path exercises the byte fields (ekey/eiv via serde_bytes) and
        // the nested timing peer list.
        let req = new_base_setup(
            "rtsp://10.0.0.2/1",
            "AA:BB:CC:DD:EE:FF",
            "00112233-4455-6677-8899-AABBCCDDEEFF",
            "PTP",
            0,
            &["10.0.0.2".to_string()],
        )
        .unwrap();
        let body = req.body.expect("body present");
        let value: plist::Value = plist::from_bytes(&body).expect("valid binary plist");
        let dict = value.as_dictionary().expect("dict root");
        // ekey must be Data (serde_bytes), not an integer array.
        assert!(matches!(dict.get("ekey"), Some(plist::Value::Data(_))));
        assert_eq!(dict.get("timingProtocol").and_then(|v| v.as_string()), Some("PTP"));
    }

    #[test]
    fn stream_setup_roundtrips_shk() {
        let req = new_stream_setup(
            "rtsp://10.0.0.2/1",
            &[0u8; 32],
            Some(&[0x12, 0x10]),
            103,
            1024,
            44100,
            55555,
            AUDIO_FORMAT_AAC_44100_2,
            4,
            7,
            None,
            None,
        )
        .unwrap();
        let value: plist::Value = plist::from_bytes(&req.body.unwrap()).unwrap();
        let streams = value
            .as_dictionary()
            .and_then(|d| d.get("streams"))
            .and_then(|v| v.as_array())
            .unwrap();
        let s = streams[0].as_dictionary().unwrap();
        assert!(matches!(s.get("shk"), Some(plist::Value::Data(_))));
        assert_eq!(s.get("type").and_then(|v| v.as_signed_integer()), Some(103));
    }

    #[test]
    fn group_session_setup_serializes_ekey_eiv_et() {
        let params = GroupSetupParams {
            device_id: "AA:BB:CC:DD:EE:FF".to_string(),
            mac_address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "test".to_string(),
            local_addresses: vec!["10.0.0.2".to_string()],
            clock_id: 123,
            group_uuid: "00112233-4455-6677-8899-AABBCCDDEEFF".to_string(),
            session_uuid: "11112222-3333-4444-5555-666677778888".to_string(),
            session_correlation_uuid: "22223333-4444-5555-6666-777788889999".to_string(),
            hijack_id: "33334444-5555-6666-7777-88889999AAAA".to_string(),
            source_ipv4: "10.0.0.2".to_string(),
            group_ipv4: "232.1.2.3:8738".to_string(),
            group_ipv6: None,
            source_ipv6: None,
            group_encryption_key: vec![0u8; 32],
            et: Some(32),
            ekey: Some(vec![0u8; 72]),
            eiv: Some(vec![0u8; 16]),
        };
        let req = new_group_session_setup("rtsp://10.0.0.2/1", &params).unwrap();
        let value: plist::Value = plist::from_bytes(&req.body.unwrap()).unwrap();
        let dict = value.as_dictionary().unwrap();
        assert_eq!(dict.get("et").and_then(|v| v.as_signed_integer()), Some(32));
        assert!(matches!(dict.get("ekey"), Some(plist::Value::Data(_))));
        assert!(matches!(dict.get("eiv"), Some(plist::Value::Data(_))));
    }
}
