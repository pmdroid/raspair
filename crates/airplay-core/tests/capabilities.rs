use airplay_core::protocol::{
    parse_txt_record, AdvertisedAuth, AuthRequirement as A, StreamCandidate as S,
};
use airplay_core::pairing::{MemoryStore, PinPairings};
use airplay_core::{Client, Device, Session, SessionError, SessionOptions};

const HOMEPOD: &[&str] = &["deviceid=AA:BB:CC:00:00:01", "features=0x4A7FDFD5,0x3C155FDE", "flags=0x18644", "model=AudioAccessory5,1"];
const APPLE_TV: &[&str] = &["deviceid=AA:BB:CC:00:00:02", "features=0x4A7FDFD5,0xBC157FDE", "flags=0x244", "model=AppleTV11,1"];
const SHAIRPORT: &[&str] = &["deviceid=AA:BB:CC:00:00:03", "features=0x405C4A00,0x1C340", "flags=0x4", "model=Shairport Sync"];

fn txt(base: &[&str], extra: &[&str]) -> Vec<String> {
    base.iter().chain(extra).map(|s| s.to_string()).collect()
}

fn without(base: &[&str], key: &str) -> Vec<String> {
    base.iter().filter(|s| !s.starts_with(key)).map(|s| s.to_string()).collect()
}

fn unknown(features: u64, flags: Option<u64>) -> A {
    A::Unknown(AdvertisedAuth { features, flags, password_required: false })
}

#[test]
fn advertised_capabilities_resolve_typed_routes() {
    let atv_features = 0xBC157FDE_4A7FDFD5;
    let cases: Vec<(&str, Vec<String>, A, Vec<S>)> = vec![
        ("homepod", txt(HOMEPOD, &[]), A::HapTransient, vec![S::Buffered, S::Realtime]),
        ("apple tv", txt(APPLE_TV, &[]), A::HapPersistent, vec![S::Buffered, S::Realtime]),
        ("apple tv pin", txt(APPLE_TV, &["flags=0x24c"]), A::HapPersistent, vec![S::Buffered, S::Realtime]),
        ("apple tv password", txt(APPLE_TV, &["flags=0x2c4"]), A::Password, vec![S::Buffered, S::Realtime]),
        ("apple tv password cleared", txt(APPLE_TV, &["flags=0x244", "pw=false"]), A::HapPersistent, vec![S::Buffered, S::Realtime]),
        ("homepod password", txt(HOMEPOD, &["pw=true"]), A::HapTransientPassword, vec![S::Buffered, S::Realtime]),
        ("homepod pin", txt(HOMEPOD, &["flags=0x1864c"]), A::HapPersistent, vec![S::Buffered, S::Realtime]),
        ("third-party ap2", txt(SHAIRPORT, &[]), A::HapTransient, vec![S::Buffered, S::Realtime]),
        ("third-party password", txt(SHAIRPORT, &["flags=0x84"]), A::HapTransientPassword, vec![S::Buffered, S::Realtime]),
        ("third-party one-time", txt(SHAIRPORT, &["flags=0x204"]), A::HapPersistent, vec![S::Buffered, S::Realtime]),
        ("unknown feature bits", txt(SHAIRPORT, &["features=0x405C4A00,0x3001C340"]), A::HapTransient, vec![S::Buffered, S::Realtime]),
        ("unified auth", txt(&["features=0x200,0x80000", "flags=0x4"], &[]), A::UnifiedAuth, vec![S::Realtime]),
        ("unknown ap2 auth", txt(&["features=0x0,0x100", "flags=0x4"], &[]), unknown(1 << 40, Some(4)), vec![S::Buffered]),
        ("apple tv missing flags", without(APPLE_TV, "flags"), unknown(atv_features, None), vec![S::Buffered, S::Realtime]),
        ("homepod missing flags", without(HOMEPOD, "flags"), A::HapTransient, vec![S::Buffered, S::Realtime]),
        ("model-only homepod", txt(&["model=AudioAccessory1,1"], &[]), A::HapTransient, vec![]),
        ("apple tv model alone", txt(&["model=AppleTV5,3"], &[]), unknown(0, None), vec![]),
        ("empty record", vec![], unknown(0, None), vec![]),
        ("legacy raop", txt(&["am=AirPort4,107", "et=0,1", "sf=0x4"], &[]), A::None, vec![S::Realtime]),
        ("legacy raop features", txt(&["am=AirPort10,115", "ft=0x5A7FFFF7,0xE", "sf=0x4"], &[]), A::None, vec![S::Realtime]),
        ("legacy raop pin", txt(&["am=AirPort4,107", "sf=0xc"], &[]), A::LegacyPin, vec![S::Realtime]),
        ("legacy raop password", txt(&["am=AirPort4,107", "sf=0x4", "pw=true"], &[]), A::Password, vec![S::Realtime]),
    ];
    for (name, record, auth, streams) in cases {
        let route = parse_txt_record(&record).route();
        assert_eq!(route.auth, auth, "{name}");
        assert_eq!(route.streams, streams, "{name}");
        let rates: &[u32] = match (streams.contains(&S::Realtime), name.starts_with("legacy")) {
            (false, _) => &[],
            (true, false) => &[48000],
            (true, true) => &[44100],
        };
        assert_eq!(route.realtime_rates, rates, "{name}");
    }
}

fn device(record: &[&str]) -> Device {
    Device {
        name: "receiver".into(),
        host: "127.0.0.1".into(),
        port: 9,
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: parse_txt_record(&txt(record, &[])),
        last_seen: 0,
    }
}

#[test]
fn unsupported_auth_is_rejected_before_any_exchange() {
    let rejected = [
        txt(APPLE_TV, &["flags=0x2c4"]),
        without(APPLE_TV, "flags"),
        txt(&["am=AirPort4,107", "sf=0x4"], &[]),
        txt(&["features=0x200,0x80000", "flags=0x4"], &[]),
        txt(&["features=0x0,0x100", "flags=0x4"], &[]),
    ];
    for record in rejected {
        let refs: Vec<&str> = record.iter().map(String::as_str).collect();
        let dev = device(&refs);
        let expected = SessionError::UnsupportedAuthentication(dev.route().auth);
        let session_error = if dev.record.is_legacy_raop() { SessionError::UnsupportedProfile } else { expected.clone() };
        match Session::connect(&dev, "3939", SessionOptions::default()) {
            Err(e) => assert_eq!(e, session_error),
            Ok(_) => panic!("connected with {record:?}"),
        }
        if !matches!(dev.route().auth, A::None | A::Password) {
            let err = Client::new().connect(&dev, "3939").unwrap_err();
            assert_eq!(err.downcast_ref::<SessionError>(), Some(&expected));
        }
        let err = Client::pair(&dev, "3939").unwrap_err();
        assert_eq!(err.downcast_ref::<SessionError>(), Some(&expected));
    }
    let legacy = device(&["am=AirPort4,107", "sf=0xc"]);
    assert_eq!(legacy.route().pairing_method(), Ok(airplay_core::protocol::PairingMethod::LegacyPin));
    assert_eq!(
        Session::connect(&legacy, "3939", SessionOptions::default()).err(),
        Some(SessionError::UnsupportedProfile)
    );
    let err = Client::pair(&device(HOMEPOD), "3939").unwrap_err();
    assert_eq!(err.downcast_ref::<SessionError>(), Some(&SessionError::UnsupportedAuthentication(A::HapTransient)));
    let pairings = PinPairings::new(std::sync::Arc::new(MemoryStore::default()));
    let err = Client::begin_pairing(&device(HOMEPOD), &pairings).unwrap_err();
    assert_eq!(err.downcast_ref::<SessionError>(), Some(&SessionError::UnsupportedAuthentication(A::HapTransient)));
}

#[test]
fn pin_source_follows_advertised_pairing_status() {
    assert!(parse_txt_record(&txt(APPLE_TV, &[])).displays_pin());
    assert!(parse_txt_record(&txt(APPLE_TV, &["flags=0x4c"])).displays_pin());
    assert!(!parse_txt_record(&txt(APPLE_TV, &["flags=0x44"])).displays_pin());
}
