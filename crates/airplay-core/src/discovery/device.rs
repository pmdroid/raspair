//! A discovered AirPlay receiver and convenience accessors over its TXT record.

use crate::protocol::{Route, TxtRecord};
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
    pub record: TxtRecord,
    pub last_seen: i64,
}

impl Device {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Best routable address string for dialing (prefers IPv4).
    pub fn ip(&self) -> Option<String> {
        if let Some(v4) = self.ipv4 {
            Some(v4.to_string())
        } else {
            self.ipv6.map(|v| v.to_string())
        }
    }

    pub fn is_apple_tv(&self) -> bool {
        self.record.is_apple_tv()
    }
    pub fn is_homepod(&self) -> bool {
        self.record.is_homepod()
    }
    pub fn supports_ap2(&self) -> bool {
        self.record.features.supports_airplay_audio()
    }
    pub fn supports_buffered_audio(&self) -> bool {
        self.record.features.supports_buffered_audio()
    }
    pub fn supports_ptp(&self) -> bool {
        self.record.features.supports_ptp()
    }
    pub fn tight_sync_id(&self) -> &str {
        &self.record.tight_sync_id
    }
    pub fn route(&self) -> Route {
        self.record.route()
    }
}

pub fn merge_device(devices: &mut Vec<Device>, device: Device) {
    let id = device.record.device_id.clone();
    for other in devices.iter_mut() {
        if !id.is_empty() && other.record.device_id != id && other.port == device.port && other.ip().is_some() && other.ip() == device.ip() {
            other.ipv4 = None;
            other.ipv6 = None;
        }
    }
    let slot = devices.iter_mut().find(|e| {
        if id.is_empty() {
            e.record.device_id.is_empty() && e.name == device.name && e.host == device.host && e.port == device.port
        } else {
            e.record.device_id == id
        }
    });
    match slot {
        Some(slot) => *slot = device,
        None => devices.push(device),
    }
}
