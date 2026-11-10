use airplay_core::pairing::{AuthMethod, CredentialStore, FileStore, Identity, MemoryStore, PairingRecord, Stored};
use airplay_core::{Client, Device, Session, SessionError, SessionOptions};
use std::net::TcpListener;
use std::sync::Arc;

fn apple_tv(port: u16, id: &str) -> Device {
    Device {
        name: "Living Room".into(),
        host: "localhost".into(),
        port,
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: airplay_core::protocol::parse_txt_record(&[
            format!("deviceid={id}"),
            "features=0x4A7FDFD5,0xBC157FDE".into(),
            "flags=0x244".into(),
            "model=AppleTV11,1".into(),
        ]),
        last_seen: 0,
    }
}

fn record(id: &str) -> PairingRecord {
    PairingRecord {
        method: AuthMethod::PairSetupPin,
        receiver_id: id.into(),
        controller_id: "controller".into(),
        accessory_id: "accessory".into(),
        controller_key: Identity::generate(),
        accessory_public_key: Identity::generate().public_key(),
        password: None,
    }
}

fn closing_receiver() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (port, std::thread::spawn(move || drop(listener.accept().unwrap())))
}

#[test]
fn failed_pair_setup_keeps_prior_record() {
    let store = MemoryStore::default();
    let prior = record("aa:bb:cc:dd:ee:ff");
    store.save(&prior).unwrap();
    let (port, receiver) = closing_receiver();
    assert!(Client::pair_with_store(&apple_tv(port, "aa:bb:cc:dd:ee:ff"), "1234", &store).is_err());
    receiver.join().unwrap();
    match store.load("aa:bb:cc:dd:ee:ff").unwrap() {
        Stored::Paired(r) => assert_eq!(r.controller_key.seed(), prior.controller_key.seed()),
        other => panic!("prior record lost: {other:?}"),
    }
}

#[test]
fn legacy_key_does_not_establish_trust() {
    let dir = std::env::temp_dir().join(format!("airplay-legacy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = dir.join("aa:bb:cc:dd:ee:01.pem");
    std::fs::write(&legacy, b"-----BEGIN ED25519 PRIVATE KEY-----\n").unwrap();
    let options = SessionOptions { credentials: Some(Arc::new(FileStore::new(&dir))), ..Default::default() };
    let (port, receiver) = closing_receiver();
    let result = Session::connect(&apple_tv(port, "aa:bb:cc:dd:ee:01"), "1234", options);
    assert_eq!(result.err(), Some(SessionError::ChallengeRequired));
    receiver.join().unwrap();
    assert!(legacy.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
