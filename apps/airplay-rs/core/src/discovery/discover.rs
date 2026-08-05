//! mDNS discovery of `_airplay._tcp` receivers via mdns-sd.

use super::device::Device;
use crate::protocol::parse_txt_record;
use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SERVICE: &str = "_airplay._tcp.local.";

/// Browse for AirPlay receivers for `timeout`, returning one [`Device`] per
/// unique host:port.
pub fn discover(timeout: Duration) -> Result<Vec<Device>> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let receiver = daemon.browse(SERVICE).context("browse mDNS")?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut devices = Vec::new();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let fullname = info.get_fullname().to_string();
                let key = fullname.to_ascii_lowercase();
                if !seen.insert(key) {
                    continue;
                }

                let name = fullname
                    .split("._airplay._tcp")
                    .next()
                    .unwrap_or(&fullname)
                    .to_string();

                let txt: Vec<String> = info
                    .get_properties()
                    .iter()
                    .map(|p| format!("{}={}", p.key(), p.val_str()))
                    .collect();
                let record = parse_txt_record(&txt);

                let mut ipv4 = None;
                let mut ipv6 = None;
                for addr in info.get_addresses() {
                    match addr {
                        IpAddr::V4(v4) if ipv4.is_none() => ipv4 = Some(*v4),
                        IpAddr::V6(v6) if ipv6.is_none() => ipv6 = Some(*v6),
                        _ => {}
                    }
                }

                devices.push(Device {
                    name,
                    host: info.get_hostname().trim_end_matches('.').to_string(),
                    port: info.get_port(),
                    ipv4,
                    ipv6,
                    record,
                    last_seen: now_unix(),
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();
    Ok(devices)
}

/// Case-insensitive match on device name or hostname.
pub fn find_device<'a>(devices: &'a [Device], name: &str) -> Option<&'a Device> {
    let n = name.to_ascii_lowercase();
    devices.iter().find(|d| {
        let clean = d
            .name
            .replace("\\ ", " ")
            .replace("\\(", "(")
            .replace("\\)", ")");
        clean.to_ascii_lowercase().contains(&n) || d.host.to_ascii_lowercase().contains(&n)
    })
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}
