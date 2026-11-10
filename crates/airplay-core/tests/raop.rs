use airplay_core::client::AudioCodec;
use airplay_core::pairing::{AuthMethod, CredentialStore, FileStore, Stored};
use airplay_core::{Device, Session, SessionError, SessionOptions, SessionProfile, StreamType};
use md5::{Digest, Md5};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const PASSWORD: &str = "fixture-password";

struct Request {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: String,
}

struct Receiver {
    port: u16,
    data: UdpSocket,
    control: UdpSocket,
    requests: JoinHandle<Vec<Request>>,
}

fn digest_ok(auth: Option<&String>) -> bool {
    let md5 = |s: String| hex::encode(Md5::digest(s.as_bytes()));
    let ha1 = md5(format!("iTunes:raop:{PASSWORD}"));
    let expected = md5(format!("{ha1}:n1:{}", md5("OPTIONS:*".into())));
    auth.is_some_and(|a| a.contains(&format!("response=\"{expected}\"")))
}

fn receiver(password: bool) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let data = UdpSocket::bind("127.0.0.1:0").unwrap();
    let control = UdpSocket::bind("127.0.0.1:0").unwrap();
    for s in [&data, &control] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }
    let ports = (data.local_addr().unwrap().port(), control.local_addr().unwrap().port());
    let port = listener.local_addr().unwrap().port();
    let requests = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut seen = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return seen;
            }
            let mut parts = line.split(' ');
            let (method, url) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                match h.trim_end().split_once(':') {
                    Some((k, v)) => headers.insert(k.to_ascii_lowercase(), v.trim().to_string()),
                    None => break,
                };
            }
            let mut body = vec![0; headers.get("content-length").map_or(0, |v| v.parse().unwrap())];
            reader.read_exact(&mut body).unwrap();
            let cseq = headers["cseq"].clone();
            let reply = match method.as_str() {
                "OPTIONS" if password && !digest_ok(headers.get("authorization")) => {
                    "401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"raop\", nonce=\"n1\"".to_string()
                }
                "SETUP" => format!("200 OK\r\nSession: DEADBEEF;timeout=60\r\nTransport: RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port=9", ports.0, ports.1),
                _ => "200 OK".to_string(),
            };
            writer.write_all(format!("RTSP/1.0 {reply}\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n").as_bytes()).unwrap();
            seen.push(Request { method, url, headers, body: String::from_utf8(body).unwrap() });
        }
    });
    Receiver { port, data, control, requests }
}

fn device(port: u16, txt: &[&str]) -> Device {
    let mut record = vec!["deviceid=aa:00:00:00:00:07".to_string(), "am=AirPort4,107".into(), "sf=0x4".into()];
    record.extend(txt.iter().map(|s| s.to_string()));
    Device {
        name: "Legacy".into(),
        host: "localhost".into(),
        port,
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: airplay_core::protocol::parse_txt_record(&record),
        last_seen: 0,
    }
}

fn realtime(options: SessionOptions) -> SessionOptions {
    SessionOptions { profile: SessionProfile::realtime(44100, 16).unwrap(), ..options }
}

fn recv(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 4096];
    let n = sock.recv(&mut buf).unwrap();
    buf[..n].to_vec()
}

fn bits(bytes: &[u8], start: usize, n: usize) -> u32 {
    (start..start + n).fold(0, |v, i| v << 1 | ((bytes[i / 8] >> (7 - i % 8)) & 1) as u32)
}

fn alac_escape_samples(frame: &[u8]) -> Vec<i16> {
    assert_eq!((bits(frame, 0, 3), bits(frame, 3, 4), bits(frame, 7, 12), bits(frame, 19, 4)), (1, 0, 0, 1));
    let samples: Vec<i16> = (0..352 * 2).map(|i| bits(frame, 23 + i * 16, 16) as u16 as i16).collect();
    assert_eq!(bits(frame, 23 + 352 * 32, 3), 7);
    assert_eq!(frame.len(), (23 + 352 * 32 + 3usize).div_ceil(8));
    samples
}

fn fixture() -> (Vec<f32>, Vec<i16>) {
    let ints: Vec<i16> = (0..352 * 2 * 2).map(|i| ((i * 977) % 65536) as u16 as i16).collect();
    (ints.iter().map(|&v| v as f32 / 32768.0).collect(), ints)
}

#[test]
fn loopback_raop_receiver_gets_sdp_ports_and_exact_plaintext_rtp() {
    for (cn, codec) in [("cn=0,1", AudioCodec::Alac), ("cn=0", AudioCodec::Pcm)] {
        let rx = receiver(false);
        let mut session = Session::connect(&device(rx.port, &[cn, "et=0,1"]), "", realtime(SessionOptions::default())).unwrap();
        let effective = session.profile().unwrap().effective;
        assert_eq!((effective.transport, effective.codec, effective.sample_rate, effective.valid_bits), (StreamType::Realtime, codec, 44100, 16));
        let (samples, ints) = fixture();
        assert_eq!(session.write_realtime_samples(&samples).unwrap().packets, 2);
        let mut previous: Option<(u16, u32)> = None;
        for (i, expected) in ints.chunks(704).enumerate() {
            let packet = recv(&rx.data);
            assert_eq!((packet[0], packet[1]), (0x80, if i == 0 { 0xE0 } else { 0x60 }));
            let (seq, ts) = (u16::from_be_bytes([packet[2], packet[3]]), u32::from_be_bytes(packet[4..8].try_into().unwrap()));
            if let Some((s, t)) = previous {
                assert_eq!((seq, ts), (s.wrapping_add(1), t.wrapping_add(352)));
            }
            previous = Some((seq, ts));
            let payload = match codec {
                AudioCodec::Alac => alac_escape_samples(&packet[12..]),
                _ => packet[12..].as_chunks::<2>().0.iter().map(|b| i16::from_be_bytes(*b)).collect(),
            };
            assert_eq!(payload, expected, "{cn} packet {i}");
        }
        let mut sync = [0u8; 20];
        let (_, from) = rx.control.recv_from(&mut sync).unwrap();
        assert_eq!(&sync[..4], &[0x90, 0xD4, 0, 7]);
        session.stop().unwrap();

        let seen = rx.requests.join().unwrap();
        let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(methods, ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "SET_PARAMETER", "FLUSH", "TEARDOWN"]);
        let url = &seen[1].url;
        assert!(seen[1..].iter().all(|r| &r.url == url) && url.starts_with("rtsp://127.0.0.1/"));
        let session_id = url.rsplit('/').next().unwrap();
        let format = match codec {
            AudioCodec::Alac => "a=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n",
            _ => "a=rtpmap:96 L16/44100/2\r\n",
        };
        let sdp = format!("v=0\r\no=iTunes {session_id} 0 IN IP4 127.0.0.1\r\ns=iTunes\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n{format}");
        assert_eq!((seen[1].headers["content-type"].as_str(), seen[1].body.as_str()), ("application/sdp", sdp.as_str()));
        let transport = &seen[2].headers["transport"];
        assert!(transport.starts_with("RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port="));
        assert_eq!(sync_source_port(transport), from.port());
        assert!(seen[3..].iter().all(|r| r.headers.get("session").map(String::as_str) == Some("DEADBEEF")));
        let (first_seq, first_ts) = previous.map(|(s, t)| (s.wrapping_sub(1), t.wrapping_sub(352))).unwrap();
        assert_eq!(seen[5].headers["rtp-info"], format!("seq={first_seq};rtptime={first_ts}"));
        assert_eq!(seen[3].headers["range"], "npt=0-");
        assert_eq!(seen[4].body, "volume: -30\r\n");
    }
}

fn sync_source_port(transport: &str) -> u16 {
    transport.split(';').find_map(|p| p.strip_prefix("control_port=")).unwrap().parse().unwrap()
}

#[test]
fn legacy_receivers_need_explicit_realtime_and_unencrypted_support() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let cases = [
        (device(port, &["cn=0,1", "et=0,1"]), SessionOptions::default()),
        (device(port, &["cn=0,1", "et=1"]), realtime(SessionOptions::default())),
        (device(port, &["cn=0,1"]), realtime(SessionOptions::default())),
        (device(port, &["cn=0,1", "et=0"]), SessionOptions { profile: SessionProfile::realtime(48000, 16).unwrap(), ..Default::default() }),
    ];
    for (i, (dev, options)) in cases.into_iter().enumerate() {
        let expected = if i == 3 { SessionError::RealtimeRate { requested: 48000 } } else { SessionError::UnsupportedProfile };
        assert_eq!(Session::connect(&dev, "", options).err(), Some(expected), "case {i}");
    }
    assert!(listener.accept().is_err());
}

#[test]
fn password_is_saved_and_reused_by_a_fresh_store_on_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    for supplied in [Some(PASSWORD.to_string()), None] {
        let rx = receiver(true);
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(dir.path()));
        let options = realtime(SessionOptions { credentials: Some(store), password: supplied, ..Default::default() });
        let mut session = Session::connect(&device(rx.port, &["cn=1", "et=0", "pw=true"]), "", options).unwrap();
        session.stop().unwrap();
        let seen = rx.requests.join().unwrap();
        assert_eq!(seen.iter().filter(|r| r.method == "OPTIONS").count(), 2);
        assert!(seen.iter().any(|r| r.method == "RECORD"));
    }
    match FileStore::new(dir.path()).load("aa:00:00:00:00:07").unwrap() {
        Stored::Paired(record) => assert_eq!((record.method, record.password.as_deref()), (AuthMethod::Password, Some(PASSWORD))),
        _ => panic!("password not saved"),
    }
}
