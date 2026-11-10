use airplay_core::pairing::{AuthMethod, CredentialStore, FileStore, Identity, MemoryStore, PairingRecord, Stored};
use airplay_core::rtsp::request::AuthError;
use airplay_core::{Client, Device};
use md5::{Digest, Md5};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread::JoinHandle;

const PASSWORD: &str = "fixture-password";
const REALM: &str = "raop";

#[derive(Debug)]
struct Seen {
    line: String,
    cseq: String,
    authorization: Option<String>,
}

fn device(port: u16, id: &str, txt: &[&str]) -> Device {
    let mut record = vec![format!("deviceid={id}")];
    record.extend(txt.iter().map(|s| s.to_string()));
    Device {
        name: "Fixture".into(),
        host: "localhost".into(),
        port,
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: airplay_core::protocol::parse_txt_record(&record),
        last_seen: 0,
    }
}

fn open_device(port: u16) -> Device {
    device(port, "aa:00:00:00:00:01", &["am=AirPort4,107", "sf=0x4"])
}

fn password_device(port: u16, id: &str) -> Device {
    device(port, id, &["am=AirPort4,107", "sf=0x4", "pw=true"])
}

fn md5(s: String) -> String {
    hex::encode(Md5::digest(s.as_bytes()))
}

fn expected_response(method: &str, uri: &str, nonce: &str, password: &str) -> String {
    let ha1 = md5(format!("iTunes:{REALM}:{password}"));
    md5(format!("{ha1}:{nonce}:{}", md5(format!("{method}:{uri}"))))
}

fn valid(seen: &Seen, nonce: &str) -> bool {
    let auth = seen.authorization.as_deref().unwrap_or_default();
    auth.contains(&format!("nonce=\"{nonce}\""))
        && auth.contains(&format!("response=\"{}\"", expected_response("OPTIONS", "*", nonce, PASSWORD)))
}

fn receiver<F>(mut respond: F) -> (u16, JoinHandle<Vec<Seen>>)
where
    F: FnMut(usize, &Seen) -> String + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut seen = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return seen;
            }
            let mut req = Seen { line: line.trim_end().into(), cseq: String::new(), authorization: None };
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                match k {
                    "CSeq" => req.cseq = v.trim().into(),
                    "Authorization" => req.authorization = Some(v.trim().into()),
                    _ => {}
                }
            }
            let reply = respond(seen.len(), &req);
            let reply = reply.replace("{cseq}", &req.cseq);
            seen.push(req);
            if writer.write_all(reply.as_bytes()).is_err() {
                return seen;
            }
        }
    });
    (port, handle)
}

fn ok() -> String {
    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n".into()
}

fn challenge(params: &str) -> String {
    format!("RTSP/1.0 401 Unauthorized\r\nCSeq: {{cseq}}\r\nWWW-Authenticate: Digest {params}\r\n\r\n")
}

fn digest_receiver(nonces: &'static [&'static str]) -> (u16, JoinHandle<Vec<Seen>>) {
    receiver(move |n, req| {
        if valid(req, nonces[nonces.len() - 1]) {
            return ok();
        }
        let stale = if req.authorization.is_some() { ", stale=true" } else { "" };
        challenge(&format!("realm=\"{REALM}\", nonce=\"{}\"{stale}", nonces[n.min(nonces.len() - 1)]))
    })
}

fn connect(dev: &Device, pin: &str, store: Arc<dyn CredentialStore>) -> anyhow::Result<()> {
    let mut client = Client::new();
    client.set_credential_store(store);
    client.connect(dev, pin)
}

fn auth_error(result: anyhow::Result<()>) -> AuthError {
    let err = result.expect_err("connect should fail");
    assert!(!format!("{err:?}").contains(PASSWORD));
    err.downcast_ref::<AuthError>().cloned().unwrap_or_else(|| panic!("unexpected error {err:?}"))
}

#[test]
fn open_receiver_needs_no_pairing_record_or_authorization() {
    let (port, rx) = receiver(|_, _| ok());
    let store = Arc::new(MemoryStore::default());
    connect(&open_device(port), "", store.clone()).unwrap();
    let seen = rx.join().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].line, "OPTIONS * RTSP/1.0");
    assert!(seen[0].authorization.is_none());
    assert!(matches!(store.load("aa:00:00:00:00:01").unwrap(), Stored::Missing));
}

#[test]
fn open_receiver_demanding_a_password_fails_once() {
    let (port, rx) = receiver(|_, _| challenge(&format!("realm=\"{REALM}\", nonce=\"n1\"")));
    assert_eq!(auth_error(connect(&open_device(port), "", Arc::new(MemoryStore::default()))), AuthError::PasswordRequired);
    assert_eq!(rx.join().unwrap().len(), 1);
}

#[test]
fn correct_password_authenticates_and_is_saved() {
    let (port, rx) = digest_receiver(&["n1"]);
    let store = Arc::new(MemoryStore::default());
    connect(&password_device(port, "aa:00:00:00:00:02"), PASSWORD, store.clone()).unwrap();
    let seen = rx.join().unwrap();
    assert_eq!(seen.iter().map(|s| s.cseq.as_str()).collect::<Vec<_>>(), ["1", "2"]);
    assert!(seen[0].authorization.is_none());
    assert!(!seen[1].authorization.as_deref().unwrap().contains(PASSWORD));
    let Stored::Paired(record) = store.load("aa:00:00:00:00:02").unwrap() else { panic!("not saved") };
    assert_eq!((record.method, record.password.as_deref()), (AuthMethod::Password, Some(PASSWORD)));
    assert_eq!(record.accessory_public_key, None);
}

#[test]
fn incorrect_password_stops_after_one_authenticated_attempt() {
    let (port, rx) = receiver(|_, _| challenge(&format!("realm=\"{REALM}\", nonce=\"n1\"")));
    let store = Arc::new(MemoryStore::default());
    let dev = password_device(port, "aa:00:00:00:00:03");
    assert_eq!(auth_error(connect(&dev, "wrong", store.clone())), AuthError::Rejected);
    assert_eq!(rx.join().unwrap().len(), 2);
    assert!(matches!(store.load("aa:00:00:00:00:03").unwrap(), Stored::Missing));
}

#[test]
fn stale_nonce_is_retried_with_the_fresh_nonce() {
    let (port, rx) = digest_receiver(&["n1", "n2"]);
    connect(&password_device(port, "aa:00:00:00:00:04"), PASSWORD, Arc::new(MemoryStore::default())).unwrap();
    let seen = rx.join().unwrap();
    assert_eq!(seen.iter().map(|s| s.cseq.as_str()).collect::<Vec<_>>(), ["1", "2", "3"]);
    assert!(valid(&seen[2], "n2"));
}

#[test]
fn repeated_stale_challenges_are_bounded() {
    let (port, rx) = receiver(|n, _| challenge(&format!("realm=\"{REALM}\", nonce=\"n{n}\", stale=true")));
    let dev = password_device(port, "aa:00:00:00:00:05");
    assert_eq!(auth_error(connect(&dev, PASSWORD, Arc::new(MemoryStore::default()))), AuthError::Rejected);
    assert_eq!(rx.join().unwrap().len(), 3);
}

#[test]
fn stale_flag_without_a_new_nonce_is_a_rejection() {
    let (port, rx) = receiver(|_, _| challenge(&format!("realm=\"{REALM}\", nonce=\"n1\", stale=true")));
    let dev = password_device(port, "aa:00:00:00:00:06");
    assert_eq!(auth_error(connect(&dev, PASSWORD, Arc::new(MemoryStore::default()))), AuthError::Rejected);
    assert_eq!(rx.join().unwrap().len(), 2);
}

#[test]
fn malformed_and_unsupported_challenges_fail_without_retry() {
    for (params, expected) in [
        (format!("realm=\"{REALM}\", nonce=\"n1"), AuthError::MalformedChallenge),
        (format!("realm=\"{REALM}\""), AuthError::MalformedChallenge),
        (format!("realm=\"{REALM}\", nonce=\"n1\", algorithm=SHA-256"), AuthError::Unsupported("algorithm SHA-256".into())),
        (format!("realm=\"{REALM}\", nonce=\"n1\", qop=\"auth\""), AuthError::Unsupported("qop".into())),
    ] {
        let reply = challenge(&params);
        let (port, rx) = receiver(move |_, _| reply.clone());
        let dev = password_device(port, "aa:00:00:00:00:07");
        assert_eq!(auth_error(connect(&dev, PASSWORD, Arc::new(MemoryStore::default()))), expected);
        assert_eq!(rx.join().unwrap().len(), 1);
    }
    let (port, rx) = receiver(|_, _| "RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: Basic realm=\"raop\"\r\n\r\n".into());
    let dev = password_device(port, "aa:00:00:00:00:07");
    assert_eq!(auth_error(connect(&dev, PASSWORD, Arc::new(MemoryStore::default()))), AuthError::Unsupported("scheme Basic".into()));
    assert_eq!(rx.join().unwrap().len(), 1);
}

#[test]
fn reloaded_password_authenticates_without_prompt_for_the_same_receiver_only() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = digest_receiver(&["n1"]);
    connect(&password_device(port, "aa:00:00:00:00:08"), PASSWORD, Arc::new(FileStore::new(dir.path()))).unwrap();
    rx.join().unwrap();

    let (port, rx) = digest_receiver(&["n2"]);
    connect(&password_device(port, "aa:00:00:00:00:08"), "", Arc::new(FileStore::new(dir.path()))).unwrap();
    assert!(valid(&rx.join().unwrap()[1], "n2"));

    let (port, rx) = digest_receiver(&["n3"]);
    let other = password_device(port, "aa:00:00:00:00:09");
    assert_eq!(auth_error(connect(&other, "", Arc::new(FileStore::new(dir.path())))), AuthError::PasswordRequired);
    assert!(rx.join().unwrap().is_empty());
}

#[test]
fn rejected_saved_password_falls_back_to_supplied_password_and_replaces_it() {
    let store = Arc::new(MemoryStore::default());
    let key = Identity::generate();
    store
        .save(&PairingRecord {
            method: AuthMethod::Password,
            receiver_id: "aa:00:00:00:00:0a".into(),
            controller_id: hex::encode(key.public_key()),
            accessory_id: String::new(),
            controller_key: key,
            accessory_public_key: None,
            password: Some("outdated".into()),
        })
        .unwrap();
    let (port, rx) = digest_receiver(&["n1"]);
    connect(&password_device(port, "aa:00:00:00:00:0a"), PASSWORD, store.clone()).unwrap();
    assert_eq!(rx.join().unwrap().len(), 4);
    let Stored::Paired(record) = store.load("aa:00:00:00:00:0a").unwrap() else { panic!("not saved") };
    assert_eq!(record.password.as_deref(), Some(PASSWORD));
}
