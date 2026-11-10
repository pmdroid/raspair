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
pub const FEATURE_SUPPORTS_COREUTILS_PAIRING: u32 = 38;
pub const FEATURE_SUPPORTS_HK_PAIRING_AND_ACCESS_CONTROL: u32 = 46;
pub const FEATURE_SUPPORTS_TRANSIENT_PAIRING: u32 = 48;

pub const STATUS_PIN_REQUIRED: u64 = 1 << 3;
pub const STATUS_PASSWORD_REQUIRED: u64 = 1 << 7;
pub const STATUS_ONE_TIME_PAIRING_REQUIRED: u64 = 1 << 9;

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
    pub fn supports_hap_pairing(&self) -> bool {
        [FEATURE_SUPPORTS_COREUTILS_PAIRING, FEATURE_SUPPORTS_HK_PAIRING_AND_ACCESS_CONTROL, FEATURE_SUPPORTS_TRANSIENT_PAIRING]
            .iter()
            .any(|b| self.has(*b))
    }
    pub fn is_airplay2(&self) -> bool {
        self.supports_hap_pairing()
            || [FEATURE_SUPPORTS_BUFFERED_AUDIO, FEATURE_SUPPORTS_PTP, FEATURE_SUPPORTS_UNIFIED_PAIR_SETUP_AND_MFI]
                .iter()
                .any(|b| self.has(*b))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedAuth {
    pub features: u64,
    pub flags: Option<u64>,
    pub password_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRequirement {
    None,
    Password,
    LegacyPin,
    HapTransient,
    HapTransientPassword,
    HapPersistent,
    UnifiedAuth,
    Unknown(AdvertisedAuth),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCandidate {
    Buffered,
    Realtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapPairing {
    Transient,
    TransientPassword,
    Persistent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub auth: AuthRequirement,
    pub streams: Vec<StreamCandidate>,
    pub realtime_rates: Vec<u32>,
}

impl Route {
    pub fn hap_pairing(&self) -> Result<HapPairing, crate::session::SessionError> {
        match self.auth {
            AuthRequirement::HapTransient => Ok(HapPairing::Transient),
            AuthRequirement::HapTransientPassword => Ok(HapPairing::TransientPassword),
            AuthRequirement::HapPersistent => Ok(HapPairing::Persistent),
            ref other => Err(crate::session::SessionError::UnsupportedAuthentication(other.clone())),
        }
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
    pub has_flags: bool,
    pub password_required: bool,
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

    fn status(&self, bit: u64) -> bool {
        self.has_flags && self.flags & bit != 0
    }

    pub fn displays_pin(&self) -> bool {
        self.status(STATUS_PIN_REQUIRED) || self.status(STATUS_ONE_TIME_PAIRING_REQUIRED)
    }

    pub fn auth_requirement(&self) -> AuthRequirement {
        let auth = self.unprotected_auth();
        if !self.password_required && !self.status(STATUS_PASSWORD_REQUIRED) {
            return auth;
        }
        match auth {
            AuthRequirement::HapTransient => AuthRequirement::HapTransientPassword,
            _ => AuthRequirement::Password,
        }
    }

    fn unprotected_auth(&self) -> AuthRequirement {
        let f = self.features;
        let pin = self.status(STATUS_PIN_REQUIRED);
        if self.is_homepod() && !pin && (f.has(FEATURE_SUPPORTS_TRANSIENT_PAIRING) || !f.supports_hap_pairing()) {
            return AuthRequirement::HapTransient;
        }
        let unknown = AuthRequirement::Unknown(AdvertisedAuth {
            features: f.0,
            flags: self.has_flags.then_some(self.flags),
            password_required: self.password_required,
        });
        if !f.is_airplay2() {
            return match (pin, f.0 != 0 || self.has_flags) {
                (true, _) => AuthRequirement::LegacyPin,
                (false, true) => AuthRequirement::None,
                (false, false) => unknown,
            };
        }
        if f.supports_hap_pairing() {
            if pin || self.status(STATUS_ONE_TIME_PAIRING_REQUIRED) {
                return AuthRequirement::HapPersistent;
            }
            if !self.has_flags {
                return unknown;
            }
            if f.has(FEATURE_SUPPORTS_TRANSIENT_PAIRING) {
                return AuthRequirement::HapTransient;
            }
            return AuthRequirement::HapPersistent;
        }
        if f.has(FEATURE_SUPPORTS_UNIFIED_PAIR_SETUP_AND_MFI) {
            return AuthRequirement::UnifiedAuth;
        }
        unknown
    }

    pub fn stream_candidates(&self) -> Vec<StreamCandidate> {
        let f = self.features;
        let mut streams = Vec::new();
        if f.supports_buffered_audio() {
            streams.push(StreamCandidate::Buffered);
        }
        if f.supports_airplay_audio() || (!f.is_airplay2() && (f.0 != 0 || self.has_flags)) {
            streams.push(StreamCandidate::Realtime);
        }
        streams
    }

    pub fn realtime_rates(&self) -> Vec<u32> {
        match (self.stream_candidates().contains(&StreamCandidate::Realtime), self.features.is_airplay2()) {
            (false, _) => Vec::new(),
            (true, true) => vec![48000],
            (true, false) => vec![44100],
        }
    }

    pub fn route(&self) -> Route {
        Route { auth: self.auth_requirement(), streams: self.stream_candidates(), realtime_rates: self.realtime_rates() }
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
    if let Some(v) = map.get("features").or_else(|| map.get("ft")) {
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
    if let Some(v) = map.get("model").or_else(|| map.get("am")) {
        r.model = v.clone();
    }
    if let Some(v) = map.get("srcvers").or_else(|| map.get("vs")) {
        r.source_version = v.clone();
    }
    if let Some(v) = map.get("protovers") {
        r.proto_version = v.clone();
    }
    if let Some(v) = map.get("flags").or_else(|| map.get("sf")) {
        if let Ok(n) = u64::from_str_radix(v.trim_start_matches("0x"), 16) {
            r.flags = n;
            r.has_flags = true;
        }
    }
    if let Some(v) = map.get("pw") {
        r.password_required = v == "1" || v.eq_ignore_ascii_case("true");
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
