//! Wire constants, feature-flag helpers, and mDNS TXT-record parsing shared
//! across the protocol layers.

use std::collections::HashMap;

pub const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

pub const AIRPLAY_SERVICE: &str = "_airplay._tcp";
pub const RAOP_SERVICE: &str = "_raop._tcp";
pub const DEFAULT_PORT: u16 = 7000;

pub const HOMEPOD_PIN: &str = "3939";

// Feature-flag bit indices (see emanuelecozzi.net/docs/airplay2).
pub const FEATURE_SUPPORTS_AIRPLAY_AUDIO: u32 = 9;
pub const FEATURE_HAS_UNIFIED_ADVERTISER_INFO: u32 = 30;
pub const FEATURE_SUPPORTS_BUFFERED_AUDIO: u32 = 40;
pub const FEATURE_SUPPORTS_PTP: u32 = 41;
pub const FEATURE_SUPPORTS_COREUTILS_PAIRING_AND_ENCRYPTION: u32 = 48;
pub const FEATURE_SUPPORTS_UNIFIED_PAIR_SETUP_AND_MFI: u32 = 51;

// Stream types carried in SETUP "type".
pub const STREAM_TYPE_REALTIME: i64 = 96;
pub const STREAM_TYPE_BUFFERED: i64 = 103;

/// 64-bit AirPlay feature bitmask advertised in the `features` TXT key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeatureFlags(pub u64);

impl FeatureFlags {
    pub fn has(&self, bit: u32) -> bool {
        self.0 & (1u64 << bit) != 0
    }
    pub fn supports_airplay_audio(&self) -> bool {
        self.has(FEATURE_SUPPORTS_AIRPLAY_AUDIO)
    }
    pub fn supports_buffered_audio(&self) -> bool {
        self.has(FEATURE_SUPPORTS_BUFFERED_AUDIO)
    }
    pub fn supports_ptp(&self) -> bool {
        self.has(FEATURE_SUPPORTS_PTP)
    }
}

/// Parsed contents of an AirPlay `_airplay._tcp` TXT record.
#[derive(Debug, Clone, Default)]
pub struct TxtRecord {
    pub device_id: String,
    pub features: FeatureFlags,
    pub group_id: String,
    pub is_group_leader: bool,
    pub parent_group_id: String,
    pub tight_sync_id: String,
    pub model: String,
    pub source_version: String,
    pub proto_version: String,
    pub flags: u64,
    pub public_key: Vec<u8>,
    pub pairing_identity: String,
    pub auth_status: i32,
}

impl TxtRecord {
    pub fn is_apple_tv(&self) -> bool {
        self.model.starts_with("AppleTV")
    }
    pub fn is_homepod(&self) -> bool {
        self.model.starts_with("AudioAccessory")
    }
    pub fn display_name(&self) -> String {
        if self.is_apple_tv() {
            format!("Apple TV ({})", self.model)
        } else if self.is_homepod() {
            "HomePod".to_string()
        } else {
            format!("AirPlay ({})", self.model)
        }
    }
}

/// Parse "key=value" TXT strings into a [`TxtRecord`].
pub fn parse_txt_record(txt: &[String]) -> TxtRecord {
    let mut map: HashMap<String, String> = HashMap::new();
    for kv in txt {
        if let Some(eq) = kv.find('=') {
            map.insert(kv[..eq].to_ascii_lowercase(), kv[eq + 1..].to_string());
        }
    }
    let mut r = TxtRecord::default();
    if let Some(v) = map.get("deviceid") {
        r.device_id = v.to_ascii_lowercase();
    }
    if let Some(v) = map.get("features") {
        // Advertised as "0x<low32>,0x<high32>"; the upper 32 bits (PTP,
        // buffered audio, ...) live in the second half.
        let parts: Vec<&str> = v.split(',').collect();
        let mut features: u64 = 0;
        if let Some(p) = parts.first() {
            let t = p.trim().trim_start_matches("0x");
            if let Ok(v) = u64::from_str_radix(t, 16) {
                features = v;
            }
        }
        if parts.len() > 1 {
            let t = parts[1].trim().trim_start_matches("0x");
            if let Ok(v) = u64::from_str_radix(t, 16) {
                features |= v << 32;
            }
        }
        r.features = FeatureFlags(features);
    }
    if let Some(v) = map.get("gid") {
        r.group_id = v.clone();
    }
    if let Some(v) = map.get("igl") {
        r.is_group_leader = v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Some(v) = map.get("pgid") {
        r.parent_group_id = v.clone();
    }
    if let Some(v) = map.get("tsid") {
        r.tight_sync_id = v.clone();
    }
    if let Some(v) = map.get("model") {
        r.model = v.clone();
    }
    if let Some(v) = map.get("srcvers") {
        r.source_version = v.clone();
    }
    if let Some(v) = map.get("protovers") {
        r.proto_version = v.clone();
    }
    if let Some(v) = map.get("flags") {
        if let Ok(n) = u64::from_str_radix(v.trim_start_matches("0x"), 16) {
            r.flags = n;
        }
    }
    if let Some(v) = map.get("pk") {
        r.public_key = v.clone().into_bytes();
    }
    if let Some(v) = map.get("pi") {
        r.pairing_identity = v.clone();
    }
    if let Some(v) = map.get("authstatus") {
        if let Ok(n) = v.parse() {
            r.auth_status = n;
        }
    }
    r
}
