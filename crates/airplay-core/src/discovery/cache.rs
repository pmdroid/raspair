//! JSON device cache (compatible with the Go reference's `devices.json`) so
//! repeated plays skip the multi-second mDNS scan.

use super::device::{merge_device, Device};
use super::discover::{discover, find_device};
use crate::protocol::{FeatureFlags, TxtRecord};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;
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

fn read_cache() -> Result<Vec<Device>> {
    let data = std::fs::read(config_path())?;
    let cached: Vec<CachedDevice> = serde_json::from_slice(&data)?;
    Ok(cached.iter().map(|c| c.to_device()).collect())
}

fn save_cache(devices: &[Device]) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cached: Vec<CachedDevice> = devices.iter().map(CachedDevice::from_device).collect();
    std::fs::write(path, serde_json::to_vec_pretty(&cached)?)?;
    Ok(())
}

fn merge(cached: Vec<Device>, fresh: Vec<Device>) -> Vec<Device> {
    let mut result = cached;
    for d in fresh {
        merge_device(&mut result, d);
    }
    result
}

/// Return cached devices when fresh; otherwise run an mDNS scan, merge, and
/// persist. When `name` is set, a scan is forced if it isn't already cached.
pub fn load_or_discover(name: &str, timeout: Duration, max_age: Duration) -> Result<Vec<Device>> {
    let cached = read_cache().unwrap_or_default();

    if !cached.is_empty() {
        let now = now_unix();
        let fresh = cached.iter().any(|d| now - d.last_seen <= max_age.as_secs() as i64);
        if fresh && name.is_empty() {
            return Ok(cached);
        }
        if !name.is_empty() && find_device(&cached, name).is_some_and(|d| d.ip().is_some() && d.record.has_flags) {
            return Ok(cached);
        }
    }

    let discovered = discover(timeout).unwrap_or_default();
    let merged = merge(cached, discovered);
    let _ = save_cache(&merged);

    if !name.is_empty() && find_device(&merged, name).is_none() {
        anyhow::bail!("device {name:?} not found (scanned {} devices)", merged.len());
    }
    Ok(merged)
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}
