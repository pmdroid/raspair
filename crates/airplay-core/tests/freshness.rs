use airplay_core::discovery::DeviceCache;
use airplay_core::protocol::parse_txt_record;
use airplay_core::session::StereoPair;
use airplay_core::Device;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REFRESH: Duration = Duration::ZERO;
const AGE: Duration = Duration::from_secs(1800);
const T: Duration = Duration::from_secs(5);

fn device(name: &str, id: &str, ip: [u8; 4], group: &str) -> Device {
    let mut txt = vec![format!("deviceid={id}"), "features=0x405C4A00,0x1C340".into(), "flags=0x4".into()];
    if !group.is_empty() {
        txt.push(format!("tsid={group}"));
    }
    Device { name: name.into(), host: format!("{name}.local"), port: 7000, ipv4: Some(Ipv4Addr::from(ip)), ipv6: None, record: parse_txt_record(&txt), last_seen: 0 }
}

struct Rig {
    cache: DeviceCache,
    scan: Arc<Mutex<Vec<Device>>>,
    scans: Arc<AtomicUsize>,
    clock: Arc<AtomicI64>,
}

impl Rig {
    fn new(dir: &Path) -> Self {
        let scan = Arc::new(Mutex::new(Vec::new()));
        let scans = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let (s, n, c) = (scan.clone(), scans.clone(), clock.clone());
        let cache = DeviceCache::new(
            dir.join("devices.json"),
            move |_| {
                n.fetch_add(1, Ordering::SeqCst);
                Ok(s.lock().unwrap().clone())
            },
            move || c.load(Ordering::SeqCst),
        );
        Self { cache, scan, scans, clock }
    }

    fn sees(&self, devices: Vec<Device>) {
        *self.scan.lock().unwrap() = devices;
    }

    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    fn ids(&mut self, name: &str, max_age: Duration) -> Vec<String> {
        let mut ids: Vec<String> = self.cache.lookup(name, T, max_age).unwrap().into_iter().map(|d| d.record.device_id).collect();
        ids.sort();
        ids
    }

    fn scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }
}

#[test]
fn refresh_shows_only_receivers_seen_in_that_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path());
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], ""), device("B", "bb:01", [10, 0, 0, 2], "")]);
    assert_eq!(rig.ids("", REFRESH), ["aa:01", "bb:01"]);

    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], "")]);
    assert_eq!(rig.ids("", REFRESH), ["aa:01"], "same-second refresh still scans");
    assert_eq!(rig.scans(), 2);
    assert_eq!(rig.ids("", AGE), ["aa:01"], "the cache does not remember B");
    assert!(rig.cache.lookup("B", T, AGE).is_err());

    rig.sees(Vec::new());
    assert!(rig.cache.lookup("", T, REFRESH).unwrap().is_empty());
    assert!(rig.cache.lookup("", T, AGE).unwrap().is_empty(), "an empty scan is not recycled into history");
}

#[test]
fn cached_lookups_enforce_freshness_per_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path());
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], ""), device("B", "bb:01", [10, 0, 0, 2], "")]);
    rig.ids("", REFRESH);
    rig.advance(60);
    assert_eq!(rig.ids("", AGE), ["aa:01", "bb:01"]);
    assert_eq!(rig.scans(), 1, "a fully fresh cache is reused");

    // A legacy cache where A is fresh and B expired must not keep B alive.
    let now = 1_000_060;
    let legacy = format!(
        r#"[{{"name":"A","host":"A.local","port":7000,"ipv4":"10.0.0.1","device_id":"aa:01","model":"","features":0,"flags":4,"last_seen":{now}}},
            {{"name":"B","host":"B.local","port":7000,"ipv4":"10.0.0.2","device_id":"bb:01","model":"","features":0,"flags":4,"last_seen":{}}}]"#,
        now - 7200
    );
    std::fs::write(dir.path().join("devices.json"), legacy).unwrap();
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], "")]);
    assert_eq!(rig.ids("", AGE), ["aa:01"]);
    assert_eq!(rig.scans(), 2);

    rig.advance(1801);
    rig.sees(Vec::new());
    assert!(rig.cache.lookup("A", T, AGE).is_err(), "an expired named receiver forces a scan");
    assert_eq!(rig.scans(), 3);
}

#[test]
fn rediscovery_at_a_new_address_replaces_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path());
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], ""), device("B", "bb:01", [10, 0, 0, 2], "")]);
    rig.ids("", REFRESH);
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], "")]);
    rig.ids("", REFRESH);

    let mut moved = device("B", "bb:01", [10, 0, 0, 9], "");
    moved.host = "B-2.local".into();
    rig.sees(vec![device("A", "aa:01", [10, 0, 0, 1], ""), moved.clone(), moved]);
    let found = rig.cache.lookup("", T, REFRESH).unwrap();
    let b: Vec<&Device> = found.iter().filter(|d| d.record.device_id == "bb:01").collect();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].ip().as_deref(), Some("10.0.0.9"));
    let named = rig.cache.lookup("bb:01", T, AGE).unwrap();
    assert_eq!(named.iter().find(|d| d.record.device_id == "bb:01").unwrap().ip().as_deref(), Some("10.0.0.9"));
}

#[test]
fn a_vanished_stereo_mate_is_not_selected_from_the_old_cache() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path());
    rig.sees(vec![device("Left", "cc:01", [10, 0, 0, 1], "grp"), device("Right", "cc:02", [10, 0, 0, 2], "grp")]);
    let both = rig.cache.lookup("", T, REFRESH).unwrap();
    assert!(StereoPair::resolve(&both, &both[0]).is_some());

    rig.sees(vec![device("Left", "cc:01", [10, 0, 0, 1], "grp")]);
    for max_age in [REFRESH, AGE] {
        let found = rig.cache.lookup("cc:01", T, max_age).unwrap();
        let left = found.iter().find(|d| d.record.device_id == "cc:01").unwrap();
        assert!(StereoPair::resolve(&found, left).is_none());
    }
}

#[test]
fn a_failed_scan_reports_the_error_instead_of_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let mut ok = DeviceCache::new(path.clone(), |_| Ok(vec![device("A", "aa:01", [10, 0, 0, 1], "")]), || 1_000);
    ok.lookup("", T, REFRESH).unwrap();
    let mut broken = DeviceCache::new(path, |_| anyhow::bail!("mDNS unavailable"), || 1_000);
    assert!(broken.lookup("", T, REFRESH).is_err());
}
