//! JSON device cache (compatible with the Go reference's `devices.json`) so
//! repeated plays skip the multi-second mDNS scan. The cache only ever holds
//! the receivers seen by the latest scan; it is not a history of devices.

use super::device::{merge_device, Device};
use super::discover::{discover, find_device};
use crate::protocol::{FeatureFlags, TxtRecord};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct CachedDevice {
    name: String,
    host: String,
    port: u16,
    #[serde(default)]
    ipv4: String,
    device_id: String,
    model: String,
    features: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    group_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    tight_sync_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flags: Option<u64>,
    #[serde(default)]
    password_required: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    raop_port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    raop_codecs: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    raop_encryption: Vec<u8>,
    last_seen: i64,
}

fn is_zero(port: &u16) -> bool {
    *port == 0
}

fn config_path() -> PathBuf {
    let dir = std::env::var("AIRPLAY_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".airplay"));
    dir.join("devices.json")
}

impl CachedDevice {
    fn to_device(&self) -> Device {
        Device {
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            ipv4: self.ipv4.parse::<Ipv4Addr>().ok(),
            ipv6: None,
            record: TxtRecord {
                device_id: self.device_id.clone(),
                features: FeatureFlags(self.features),
                model: self.model.clone(),
                group_id: self.group_id.clone(),
                tight_sync_id: self.tight_sync_id.clone(),
                flags: self.flags.unwrap_or_default(),
                has_flags: self.flags.is_some(),
                password_required: self.password_required,
                raop_port: self.raop_port,
                raop_codecs: self.raop_codecs.clone(),
                raop_encryption: self.raop_encryption.clone(),
                ..Default::default()
            },
            last_seen: self.last_seen,
        }
    }

    fn from_device(d: &Device) -> Self {
        Self {
            name: d.name.clone(),
            host: d.host.clone(),
            port: d.port,
            ipv4: d.ipv4.map(|v| v.to_string()).unwrap_or_default(),
            device_id: d.record.device_id.clone(),
            model: d.record.model.clone(),
            features: d.record.features.0,
            group_id: d.record.group_id.clone(),
            tight_sync_id: d.record.tight_sync_id.clone(),
            flags: d.record.has_flags.then_some(d.record.flags),
            password_required: d.record.password_required,
            raop_port: d.record.raop_port,
            raop_codecs: d.record.raop_codecs.clone(),
            raop_encryption: d.record.raop_encryption.clone(),
            last_seen: if d.last_seen == 0 { now_unix() } else { d.last_seen },
        }
    }
}

fn read_cache(path: &Path) -> Result<Vec<Device>> {
    let data = std::fs::read(path)?;
    let cached: Vec<CachedDevice> = serde_json::from_slice(&data)?;
    Ok(cached.iter().map(|c| c.to_device()).collect())
}

fn save_cache(path: &Path, devices: &[Device]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cached: Vec<CachedDevice> = devices.iter().map(CachedDevice::from_device).collect();
    std::fs::write(path, serde_json::to_vec_pretty(&cached)?)?;
    Ok(())
}

type Scan = Box<dyn FnMut(Duration) -> Result<Vec<Device>> + Send>;
type Clock = Box<dyn Fn() -> i64 + Send>;

/// The device cache plus the scanner that refreshes it.
///
/// A scan is authoritative: it replaces the cache, so only receivers observed
/// in the most recent scan are ever returned. Cache hits are allowed only
/// when every cached receiver is within `max_age`, so one fresh receiver
/// cannot keep an expired one (or a vanished stereo mate) alive. Pairing
/// credentials live in a separate store and are never touched here.
pub struct DeviceCache {
    path: PathBuf,
    scan: Scan,
    clock: Clock,
}

impl DeviceCache {
    /// `devices.json` under `AIRPLAY_CONFIG_DIR` (or `~/.airplay`), scanned with mDNS.
    pub fn open_default() -> Self {
        Self::new(config_path(), discover, now_unix)
    }

    /// A cache at `path` with an injected scanner and clock (unix seconds).
    pub fn new(
        path: impl Into<PathBuf>,
        scan: impl FnMut(Duration) -> Result<Vec<Device>> + Send + 'static,
        clock: impl Fn() -> i64 + Send + 'static,
    ) -> Self {
        Self { path: path.into(), scan: Box::new(scan), clock: Box::new(clock) }
    }

    /// Receivers currently available. `max_age == 0` is an explicit refresh:
    /// always scan and return exactly what that scan observed. Otherwise use
    /// the cache when every entry is fresh (and, when `name` is set, the named
    /// receiver is cached with an address), else scan.
    pub fn lookup(&mut self, name: &str, timeout: Duration, max_age: Duration) -> Result<Vec<Device>> {
        let now = (self.clock)();
        if !max_age.is_zero() {
            let cached = read_cache(&self.path).unwrap_or_default();
            let age = max_age.as_secs() as i64;
            let all_fresh = !cached.is_empty() && cached.iter().all(|d| d.last_seen <= now && now - d.last_seen <= age);
            let usable = all_fresh
                && (name.is_empty() || find_device(&cached, name).is_some_and(|d| d.ip().is_some() && d.record.has_flags));
            if usable {
                return Ok(cached);
            }
        }

        let mut seen = Vec::new();
        for mut d in (self.scan)(timeout)? {
            d.last_seen = now;
            merge_device(&mut seen, d);
        }
        let _ = save_cache(&self.path, &seen);

        if !name.is_empty() && find_device(&seen, name).is_none() {
            anyhow::bail!("device {name:?} not found (scanned {} devices)", seen.len());
        }
        Ok(seen)
    }
}

/// [`DeviceCache::lookup`] on the default cache with mDNS discovery.
pub fn load_or_discover(name: &str, timeout: Duration, max_age: Duration) -> Result<Vec<Device>> {
    DeviceCache::open_default().lookup(name, timeout, max_age)
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}
