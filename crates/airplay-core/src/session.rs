use crate::{BufferedAudioFormat, Client, Device};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("authentication failed")]
    Authentication,
    #[error("pairing challenge required")]
    ChallengeRequired,
    #[error("unsupported audio profile")]
    UnsupportedProfile,
    #[error("operation timed out")]
    Timeout,
    #[error("connection lost")]
    ConnectionLost,
    #[error("invalid input")]
    InvalidInput,
    #[error("local permission denied")]
    LocalPermissions,
    #[error("operation cancelled")]
    Cancelled,
    #[error("unsupported receiver authentication")]
    UnsupportedAuthentication(crate::protocol::AuthRequirement),
    #[error("receiver rejected request with status {status}")]
    ReceiverRejected { status: u16 },
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub enum SessionProfile {
    Realtime { sample_rate: i32 },
    Buffered(BufferedAudioFormat),
}

#[derive(Clone)]
pub struct SessionOptions {
    pub profile: SessionProfile,
    pub connection_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub pairing_challenge_expiry: Duration,
    pub volume_db: f64,
    pub cancellation: Cancellation,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            profile: SessionProfile::Buffered(BufferedAudioFormat::pcm_48000_24_stereo(true)),
            connection_timeout: Duration::from_secs(20),
            shutdown_timeout: Duration::from_secs(5),
            pairing_challenge_expiry: Duration::from_secs(120),
            volume_db: -30.0,
            cancellation: Cancellation::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connected,
    Stopped,
    Failed(SessionError),
}

pub struct Session {
    client: Option<Client>,
    options: SessionOptions,
    socket: Arc<Mutex<Option<TcpStream>>>,
    events: Vec<SessionEvent>,
}

impl Session {
    pub fn connect(device: &Device, pin: &str, options: SessionOptions) -> Result<Self, SessionError> {
        if Instant::now().checked_add(options.connection_timeout).is_none()
            || Instant::now().checked_add(options.shutdown_timeout).is_none()
            || options.connection_timeout.is_zero()
            || options.shutdown_timeout.is_zero()
            || options.pairing_challenge_expiry.is_zero()
            || !options.volume_db.is_finite()
        {
            return Err(SessionError::InvalidInput);
        }
        match &options.profile {
            SessionProfile::Realtime { sample_rate } if ![44100, 48000].contains(sample_rate) => {
                return Err(SessionError::UnsupportedProfile)
            }
            SessionProfile::Buffered(f) if f.channels != 2 || f.sample_rate <= 0 || f.samples_per_frame <= 0 => {
                return Err(SessionError::UnsupportedProfile)
            }
            _ => {}
        }
        let route = device.route();
        route.hap_pairing()?;
        let wanted = match options.profile {
            SessionProfile::Realtime { .. } => crate::protocol::StreamCandidate::Realtime,
            SessionProfile::Buffered(_) => crate::protocol::StreamCandidate::Buffered,
        };
        if !route.streams.contains(&wanted) {
            return Err(SessionError::UnsupportedProfile);
        }
        let ip = device.ip().ok_or(SessionError::InvalidInput)?;
        let addr = SocketAddr::new(ip.parse().map_err(|_| SessionError::InvalidInput)?, device.port);
        let socket = Arc::new(Mutex::new(None));
        let guard = Deadline::new(socket.clone(), options.connection_timeout, options.cancellation.clone());
        let mut client = Client::new();
        client.set_startup_volume(options.volume_db);
        client.deadline = Some((guard.end, options.cancellation.clone()));
        let result = (|| {
            let stream = loop {
                guard.check()?;
                match TcpStream::connect_timeout(&addr, Duration::from_millis(20).min(options.connection_timeout)) {
                    Ok(stream) => break stream,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
                        continue
                    }
                    Err(e) => return Err(translate(e.into())),
                }
            };
            *socket.lock().unwrap() = Some(stream.try_clone().map_err(|e| translate(e.into()))?);
            guard.check()?;
            let rtsp = crate::rtsp::Client::from_stream(&ip, device.port, stream).map_err(translate)?;
            client.connect_transport(device, pin, rtsp).map_err(translate)?;
            match &options.profile {
                SessionProfile::Realtime { sample_rate } => client.setup_realtime(*sample_rate, 2),
                SessionProfile::Buffered(format) => client.setup_buffered(format),
            }
            .map_err(translate)?;
            guard.check()
        })();
        let result = guard.check().and(result);
        drop(guard);
        if let Err(error) = result {
            if let Some(s) = socket.lock().unwrap().take() {
                let _ = s.shutdown(Shutdown::Both);
            }
            let _ = client.stop();
            return Err(error);
        }
        client.deadline = None;
        Ok(Self { client: Some(client), options, socket, events: vec![SessionEvent::Connected] })
    }

    pub fn events(&mut self) -> impl Iterator<Item = SessionEvent> + '_ {
        self.events.drain(..)
    }

    pub fn write_frame(&mut self, payload: &[u8], marker: bool) -> Result<(), SessionError> {
        if self.options.cancellation.is_cancelled() {
            self.stop()?;
            return Err(SessionError::Cancelled);
        }
        if payload.is_empty() {
            return Err(SessionError::InvalidInput);
        }
        let client = self.client.as_mut().ok_or(SessionError::ConnectionLost)?;
        let result = match self.options.profile {
            SessionProfile::Realtime { .. } => client.write_realtime_frame(payload, marker),
            SessionProfile::Buffered(_) => client.write_buffered_frame(payload),
        }
        .map_err(translate);
        if let Err(ref e) = result {
            self.events.push(SessionEvent::Failed(e.clone()));
        }
        result
    }

    pub fn begin_playback(&mut self) -> Result<(), SessionError> {
        let client = self.client.as_mut().ok_or(SessionError::ConnectionLost)?;
        if client.has_buffered() {
            client.begin_buffered_playback().map_err(translate)
        } else {
            Ok(())
        }
    }

    pub fn set_volume(&mut self, db: f64) -> Result<(), SessionError> {
        if !db.is_finite() {
            return Err(SessionError::InvalidInput);
        }
        self.client.as_mut().ok_or(SessionError::ConnectionLost)?.set_volume(db).map_err(translate)
    }

    pub fn cancel(&mut self) -> Result<(), SessionError> {
        self.options.cancellation.cancel();
        self.stop()
    }

    pub fn stop(&mut self) -> Result<(), SessionError> {
        let Some(mut client) = self.client.take() else {
            return Ok(());
        };
        let guard = Deadline::new(self.socket.clone(), self.options.shutdown_timeout, Cancellation::default());
        let result = client.stop().map_err(translate);
        if let Some(s) = self.socket.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        let result = guard.check().and(result);
        self.events.push(SessionEvent::Stopped);
        result
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Deadline {
    end: Instant,
    cancellation: Cancellation,
    done: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Deadline {
    fn new(socket: Arc<Mutex<Option<TcpStream>>>, timeout: Duration, cancellation: Cancellation) -> Self {
        let end = Instant::now() + timeout;
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let cancel = cancellation.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if cancel.is_cancelled() || Instant::now() >= end {
                    if let Some(s) = socket.lock().unwrap().as_ref() {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(2));
            }
        });
        Self { end, cancellation, done, worker: Some(worker) }
    }
    fn check(&self) -> Result<(), SessionError> {
        if self.cancellation.is_cancelled() {
            Err(SessionError::Cancelled)
        } else if Instant::now() >= self.end {
            Err(SessionError::Timeout)
        } else {
            Ok(())
        }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn translate(error: anyhow::Error) -> SessionError {
    if let Some(typed) = error.downcast_ref::<SessionError>() {
        return typed.clone();
    }
    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::PermissionDenied => SessionError::LocalPermissions,
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => SessionError::Timeout,
                std::io::ErrorKind::InvalidInput => SessionError::InvalidInput,
                _ => SessionError::ConnectionLost,
            };
        }
    }
    let text = format!("{error:#}");
    if text.contains("not paired") {
        return SessionError::ChallengeRequired;
    }
    SessionError::Authentication
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_stop_closes_socket_and_joins_deadline_worker() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut session = Session {
            client: Some(Client::new()),
            options: SessionOptions::default(),
            socket: Arc::new(Mutex::new(Some(stream))),
            events: Vec::new(),
        };
        let start = Instant::now();
        session.cancel().unwrap();
        session.stop().unwrap();
        assert!(start.elapsed() < session.options.shutdown_timeout);
        assert_eq!(session.events().collect::<Vec<_>>(), vec![SessionEvent::Stopped]);
        assert_eq!(std::io::Read::read(&mut receiver, &mut [0]).unwrap(), 0);
    }
}
