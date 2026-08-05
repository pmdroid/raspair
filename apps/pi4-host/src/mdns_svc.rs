//! mDNS: advertise `_vinylbridge._tcp` and browse AirPlay / Chromecast speakers.

use crate::control::{SpeakerTarget, TargetType};
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SERVICE_TYPE: &str = "_vinylbridge._tcp.local.";
const AIRPLAY_TYPE: &str = "_airplay._tcp.local.";
const CAST_TYPE: &str = "_googlecast._tcp.local.";

#[derive(Clone)]
pub struct MdnsHub {
    speakers: Arc<RwLock<Vec<SpeakerTarget>>>,
    last_browse: Arc<RwLock<Option<Instant>>>,
}

impl MdnsHub {
    pub fn start(hostname: &str, device_name: &str, http_port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let host = format!("{hostname}.local.");
        // Prefer a non-loopback IPv4 if available; mdns-sd needs host addresses.
        let ips = local_ipv4s();
        let props = [("path", "/"), ("proto", "vinyl"), ("ver", env!("CARGO_PKG_VERSION"))];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            device_name,
            &host,
            &ips[..],
            http_port,
            &props[..],
        )?;
        daemon.register(info)?;
        tracing::info!(
            service = SERVICE_TYPE,
            hostname,
            port = http_port,
            "mDNS: advertising Vinyl Bridge"
        );

        let speakers = Arc::new(RwLock::new(Vec::new()));
        let last_browse = Arc::new(RwLock::new(None));

        // Continuous browser for AirPlay + Cast (background).
        spawn_browser(daemon.clone(), AIRPLAY_TYPE, TargetType::Airplay, speakers.clone());
        spawn_browser(daemon.clone(), CAST_TYPE, TargetType::Chromecast, speakers.clone());

        // Keep daemon alive for process lifetime by leaking a clone.
        std::mem::forget(daemon);

        Ok(Self {
            speakers,
            last_browse,
        })
    }

    pub fn speakers(&self) -> Vec<SpeakerTarget> {
        self.speakers.read().clone()
    }

    pub fn mark_browsed(&self) {
        *self.last_browse.write() = Some(Instant::now());
    }
}

fn spawn_browser(
    daemon: ServiceDaemon,
    service_type: &'static str,
    target_type: TargetType,
    speakers: Arc<RwLock<Vec<SpeakerTarget>>>,
) {
    std::thread::Builder::new()
        .name(format!("mdns-{service_type}"))
        .spawn(move || {
            let receiver = match daemon.browse(service_type) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, service_type, "mDNS browse failed");
                    return;
                }
            };
            let mut seen: HashMap<String, SpeakerTarget> = HashMap::new();
            loop {
                match receiver.recv_timeout(Duration::from_secs(30)) {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        let name = info.get_fullname().to_string();
                        let port = info.get_port();
                        let ip = info
                            .get_addresses()
                            .iter()
                            .find_map(|a| match a {
                                IpAddr::V4(v4) => Some(v4.to_string()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        if ip.is_empty() {
                            continue;
                        }
                        let device_id = info
                            .get_properties()
                            .get("deviceid")
                            .or_else(|| info.get_properties().get("id"))
                            .map(|v| v.val_str().to_string())
                            .unwrap_or_default();
                        let model = info
                            .get_properties()
                            .get("model")
                            .or_else(|| info.get_properties().get("md"))
                            .map(|v| v.val_str().to_string());
                        let tight_sync_id = info
                            .get_properties()
                            .get("tsid")
                            .map(|v| v.val_str().to_string())
                            .unwrap_or_default();
                        let target = SpeakerTarget {
                            name: name.clone(),
                            ip,
                            device_id,
                            port,
                            target_type: target_type.clone(),
                            model,
                            tight_sync_id,
                        };
                        seen.insert(name, target);
                        let mut list: Vec<_> = seen.values().cloned().collect();
                        list.sort_by(|a, b| a.name.cmp(&b.name));
                        // Clone under read lock first — never upgrade read→write
                        // (parking_lot deadlocks; hung /api/speakers).
                        let merged = {
                            let existing = speakers.read();
                            merge_by_type(&existing, &list, &target_type)
                        };
                        *speakers.write() = merged;
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                        seen.remove(&fullname);
                        let mut list: Vec<_> = seen.values().cloned().collect();
                        list.sort_by(|a, b| a.name.cmp(&b.name));
                        let merged = {
                            let existing = speakers.read();
                            merge_by_type(&existing, &list, &target_type)
                        };
                        *speakers.write() = merged;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        // Timeout or channel closed — keep browsing.
                    }
                }
            }
        })
        .ok();
}

fn merge_by_type(
    existing: &[SpeakerTarget],
    updated_type: &[SpeakerTarget],
    target_type: &TargetType,
) -> Vec<SpeakerTarget> {
    let mut out: Vec<_> = existing
        .iter()
        .filter(|s| &s.target_type != target_type)
        .cloned()
        .collect();
    out.extend(updated_type.iter().cloned());
    out
}

fn local_ipv4s() -> Vec<IpAddr> {
    // Best-effort: hostname crate + std; fall back to 127.0.0.1.
    let mut out = Vec::new();
    if let Ok(ifaces) = local_ip_fallback() {
        out.extend(ifaces);
    }
    if out.is_empty() {
        out.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    out
}

fn local_ip_fallback() -> Result<Vec<IpAddr>> {
    // Connect UDP to a public address to discover the outbound interface IP
    // without sending packets (standard trick).
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect("8.8.8.8:80")?;
    let ip = sock.local_addr()?.ip();
    Ok(vec![ip])
}
