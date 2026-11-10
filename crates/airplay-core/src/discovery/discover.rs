//! mDNS discovery of `_airplay._tcp` receivers via mdns-sd.

use super::device::{merge_device, Device};
use crate::protocol::parse_txt_record;
use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SERVICE: &str = "_airplay._tcp.local.";
const RAOP: &str = "_raop._tcp.local.";

/// Browse for AirPlay receivers for `timeout`, returning one [`Device`] per
/// unique host:port.
pub fn discover(timeout: Duration) -> Result<Vec<Device>> {
    discover_until(timeout, &crate::Cancellation::default())
}

pub fn discover_until(timeout: Duration, cancel: &crate::Cancellation) -> Result<Vec<Device>> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let airplay = daemon.browse(SERVICE).context("browse mDNS")?;
    let raop = daemon.browse(RAOP).context("browse mDNS")?;

    let mut devices = Vec::new();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline && !cancel.is_cancelled() {
        let mut idle = true;
        for (receiver, is_raop) in [(&airplay, false), (&raop, true)] {
            while let Ok(event) = receiver.try_recv() {
                idle = false;
                if let ServiceEvent::ServiceResolved(info) = event {
                    merge_sighting(&mut devices, sighting(&info, is_raop), is_raop);
                }
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    let _ = daemon.shutdown();
    Ok(devices)
}

fn sighting(info: &ServiceInfo, is_raop: bool) -> Device {
    let fullname = info.get_fullname().to_string();
    let service = if is_raop { "._raop._tcp" } else { "._airplay._tcp" };
    let instance = fullname.split(service).next().unwrap_or(&fullname).to_string();
    let txt: Vec<String> = info.get_properties().iter().map(|p| format!("{}={}", p.key(), p.val_str())).collect();
    let mut record = parse_txt_record(&txt);
    if is_raop {
        record.raop_port = info.get_port();
    }
    let name = match instance.split_once('@') {
        Some((mac, name)) if is_raop => {
            if record.device_id.is_empty() && mac.len() == 12 {
                let bytes: Vec<String> = mac.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_string()).collect();
                record.device_id = bytes.join(":").to_ascii_lowercase();
            }
            name.to_string()
        }
        _ => instance,
    };
    let mut ipv4 = None;
    let mut ipv6 = None;
    for addr in info.get_addresses() {
        match addr {
            IpAddr::V4(v4) if ipv4.is_none() => ipv4 = Some(*v4),
            IpAddr::V6(v6) if ipv6.is_none() => ipv6 = Some(*v6),
            _ => {}
        }
    }
    Device {
        name,
        host: info.get_hostname().trim_end_matches('.').to_string(),
        port: info.get_port(),
        ipv4,
        ipv6,
        record,
        last_seen: now_unix(),
    }
}

pub fn merge_sighting(devices: &mut Vec<Device>, mut device: Device, is_raop: bool) {
    let id = device.record.device_id.clone();
    if let Some(existing) = devices.iter_mut().find(|e| !id.is_empty() && e.record.device_id == id) {
        if is_raop {
            existing.record.password_required |= device.record.password_required;
            existing.record.raop_port = device.record.raop_port;
            existing.record.raop_codecs = std::mem::take(&mut device.record.raop_codecs);
            existing.record.raop_encryption = std::mem::take(&mut device.record.raop_encryption);
            if device.ip().is_some() {
                existing.ipv4 = device.ipv4.or(existing.ipv4);
                existing.ipv6 = device.ipv6.or(existing.ipv6);
                existing.host = device.host;
            }
            existing.last_seen = device.last_seen;
            return;
        }
        device.record.password_required |= existing.record.password_required;
        device.record.raop_port = existing.record.raop_port;
        device.record.raop_codecs = std::mem::take(&mut existing.record.raop_codecs);
        device.record.raop_encryption = std::mem::take(&mut existing.record.raop_encryption);
    }
    merge_device(devices, device);
}

/// Case-insensitive match on device name or hostname.
pub fn find_device<'a>(devices: &'a [Device], name: &str) -> Option<&'a Device> {
    let n = name.to_ascii_lowercase();
    if let Some(d) = devices.iter().find(|d| !d.record.device_id.is_empty() && d.record.device_id == n) {
        return Some(d);
    }
    let clean = |d: &Device| d.name.replace("\\ ", " ").replace("\\(", "(").replace("\\)", ")").to_ascii_lowercase();
    let unique = |matches: Vec<&'a Device>| match matches.as_slice() {
        [first, rest @ ..] if rest.iter().all(|d| !d.record.device_id.is_empty() && d.record.device_id == first.record.device_id) => {
            Some(*first)
        }
        _ => None,
    };
    let exact: Vec<&Device> = devices.iter().filter(|d| clean(d) == n || d.host.to_ascii_lowercase() == n).collect();
    if !exact.is_empty() {
        return unique(exact);
    }
    unique(devices.iter().filter(|d| clean(d).contains(&n) || d.host.to_ascii_lowercase().contains(&n)).collect())
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}
