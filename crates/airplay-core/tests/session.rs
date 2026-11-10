use airplay_core::{Cancellation, Device, Session, SessionError, SessionOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn device(port: u16) -> Device {
    Device {
        name: "loopback".into(),
        host: "localhost".into(),
        port,
        ipv4: Some(std::net::Ipv4Addr::LOCALHOST),
        ipv6: None,
        record: airplay_core::protocol::TxtRecord { model: "AudioAccessory1,1".into(), ..Default::default() },
        last_seen: 0,
    }
}

fn failure(result: Result<Session, SessionError>) -> SessionError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("unexpected session"),
    }
}

#[test]
fn receiver_rejection_is_typed_and_redacted() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dev = device(listener.local_addr().unwrap().port());
    let receiver = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buf = [0; 4096];
        assert!(socket.read(&mut buf).unwrap() > 0);
        socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\n\r\nsecret-pin").unwrap();
    });
    let error = failure(Session::connect(&dev, "1234", SessionOptions::default()));
    assert_eq!(error, SessionError::ReceiverRejected { status: 403 });
    assert!(!error.to_string().contains("secret"));
    receiver.join().unwrap();
}

#[test]
fn deadline_and_cancellation_close_the_connection() {
    for cancel in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dev = device(listener.local_addr().unwrap().port());
        let cancellation = Cancellation::default();
        let token = cancellation.clone();
        let receiver = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            if cancel {
                token.cancel();
            }
            let mut buf = [0; 4096];
            while socket.read(&mut buf).unwrap_or(0) != 0 {}
        });
        let start = Instant::now();
        let error = failure(Session::connect(
            &dev,
            "1234",
            SessionOptions { connection_timeout: Duration::from_millis(80), cancellation, ..Default::default() },
        ));
        assert_eq!(error, if cancel { SessionError::Cancelled } else { SessionError::Timeout });
        assert!(start.elapsed() < Duration::from_secs(1));
        receiver.join().unwrap();
    }
}
