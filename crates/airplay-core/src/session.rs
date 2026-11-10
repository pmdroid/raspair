use crate::profile::{self, FallbackPolicy, ProfileSelection, ProfileTarget};
use crate::audio::convert::OutputFormat;
use crate::pairing::{CredentialStore, StoreError};
use crate::protocol::Route;
use crate::stream::{BufferedCodec, BufferedFeed, BufferedProducer, BufferedProgress, RealtimeProgress, RealtimeSender};
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
    #[error("receiver requires a password")]
    PasswordRequired,
    #[error("receiver asked to retry pairing later")]
    RetryAfter { seconds: Option<u64> },
    #[error("receiver is unavailable")]
    ReceiverUnavailable,
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
    #[error("realtime playback is 16-bit only but {requested}-bit was requested; use a buffered profile for 24-bit or request 16-bit realtime")]
    RealtimeBitDepth { requested: u16 },
    #[error("receiver has no validated {requested} Hz realtime profile; request a rate from Route::realtime_rates or allow a fallback")]
    RealtimeRate { requested: u32 },
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

#[derive(Debug, Clone)]
pub enum SessionProfile {
    Realtime { sample_rate: i32 },
    Buffered(BufferedAudioFormat),
}

impl SessionProfile {
    pub fn realtime(sample_rate: u32, bits: u16) -> Result<Self, SessionError> {
        if bits != 16 {
            return Err(SessionError::RealtimeBitDepth { requested: bits });
        }
        if !crate::audio::convert::OUTPUT_RATES.contains(&sample_rate) {
            return Err(SessionError::RealtimeRate { requested: sample_rate });
        }
        Ok(Self::Realtime { sample_rate: sample_rate as i32 })
    }
}

#[derive(Clone)]
pub struct SessionOptions {
    pub profile: SessionProfile,
    pub fallback: FallbackPolicy,
    pub connection_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub pairing_challenge_expiry: Duration,
    pub volume_db: f64,
    pub cancellation: Cancellation,
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub password: Option<String>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            profile: SessionProfile::Buffered(BufferedAudioFormat::pcm_48000_24_stereo(true)),
            fallback: FallbackPolicy::Forbid,
            connection_timeout: Duration::from_secs(20),
            shutdown_timeout: Duration::from_secs(5),
            pairing_challenge_expiry: Duration::from_secs(120),
            volume_db: -30.0,
            cancellation: Cancellation::default(),
            credentials: None,
            password: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connected,
    Profile(ProfileSelection),
    Buffered(BufferedProgress),
    Stopped,
    Failed(SessionError),
}

pub struct Session {
    client: Option<Client>,
    options: SessionOptions,
    socket: Arc<Mutex<Option<TcpStream>>>,
    events: Vec<SessionEvent>,
    selection: Option<ProfileSelection>,
    realtime: Option<RealtimeSender>,
    armed: bool,
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
        profile::candidates(&options.profile, &options.fallback)?;
        let route = device.route();
        route.hap_pairing()?;
        profile::preflight(&route, &options.profile, &options.fallback)?;
        let mut options = options;
        let ip = device.ip().ok_or(SessionError::InvalidInput)?;
        let addr = SocketAddr::new(ip.parse().map_err(|_| SessionError::InvalidInput)?, device.port);
        let socket = Arc::new(Mutex::new(None));
        let guard = Deadline::new(socket.clone(), options.connection_timeout, options.cancellation.clone());
        let mut client = Client::new();
        client.set_startup_volume(options.volume_db);
        if let Some(store) = &options.credentials {
            client.set_credential_store(store.clone());
        }
        if let Some(password) = &options.password {
            client.set_password(password);
        }
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
            let mut target = ClientTarget { client: &mut client, route: &route };
            let negotiated = profile::negotiate(&mut target, &options.profile, &options.fallback)?;
            guard.check()?;
            Ok(negotiated)
        })();
        let result = guard.check().and(result);
        drop(guard);
        let (effective, selection) = match result {
            Ok(negotiated) => negotiated,
            Err(error) => {
                if let Some(s) = socket.lock().unwrap().take() {
                    let _ = s.shutdown(Shutdown::Both);
                }
                let _ = client.stop();
                return Err(error);
            }
        };
        client.deadline = None;
        if let Some(s) = socket.lock().unwrap().as_ref() {
            s.set_read_timeout(Some(options.shutdown_timeout)).map_err(|e| translate(e.into()))?;
            s.set_write_timeout(Some(options.shutdown_timeout)).map_err(|e| translate(e.into()))?;
        }
        let realtime = match effective {
            SessionProfile::Realtime { sample_rate } => {
                Some(RealtimeSender::new(sample_rate as u32, client.realtime_latency().unwrap_or_default()))
            }
            SessionProfile::Buffered(_) => None,
        };
        options.profile = effective;
        let events = vec![SessionEvent::Connected, SessionEvent::Profile(selection)];
        Ok(Self { client: Some(client), options, socket, events, selection: Some(selection), realtime, armed: false })
    }

    pub fn realtime_output(&self) -> Option<OutputFormat> {
        match self.options.profile {
            SessionProfile::Realtime { sample_rate } => Some(OutputFormat { sample_rate: sample_rate as u32, bits: 16 }),
            SessionProfile::Buffered(_) => None,
        }
    }

    pub fn realtime_progress(&self) -> Option<RealtimeProgress> {
        self.realtime.as_ref().map(RealtimeSender::progress)
    }

    pub fn write_realtime_samples(&mut self, samples: &[f32]) -> Result<RealtimeProgress, SessionError> {
        self.drive_realtime(Some(samples))
    }

    pub fn finish_realtime(&mut self) -> Result<RealtimeProgress, SessionError> {
        self.drive_realtime(None)
    }

    fn drive_realtime(&mut self, samples: Option<&[f32]>) -> Result<RealtimeProgress, SessionError> {
        let cancellation = self.options.cancellation.clone();
        if cancellation.is_cancelled() {
            self.stop()?;
            return Err(SessionError::Cancelled);
        }
        let sender = self.realtime.as_mut().ok_or(SessionError::UnsupportedProfile)?;
        let client = self.client.as_mut().ok_or(SessionError::ConnectionLost)?;
        let cancelled = || cancellation.is_cancelled();
        let mut result = Ok(());
        if !self.armed {
            result = crate::client::begin_live_realtime_synchronized(std::slice::from_mut(client));
            self.armed = result.is_ok();
        }
        let result = result
            .and_then(|()| match samples {
                Some(samples) => sender.push(client, samples, &cancelled),
                None => sender.finish(client, &cancelled),
            })
            .map_err(translate);
        if let Err(ref error) = result {
            if *error != SessionError::InvalidInput {
                if *error != SessionError::Cancelled {
                    self.events.push(SessionEvent::Failed(error.clone()));
                }
                let _ = self.stop();
            }
        }
        result
    }

    pub fn buffered_source(&self) -> Result<(BufferedProducer, BufferedFeed), SessionError> {
        match &self.options.profile {
            SessionProfile::Buffered(format) => {
                BufferedCodec::of(format)?;
                Ok(crate::stream::buffered_queue(format.sample_rate as u32))
            }
            SessionProfile::Realtime { .. } => Err(SessionError::UnsupportedProfile),
        }
    }

    pub fn stream_buffered(&mut self, feed: &BufferedFeed) -> Result<BufferedProgress, SessionError> {
        let cancellation = self.options.cancellation.clone();
        let result = self.drive_buffered(feed, &cancellation);
        if let Err(ref error) = result {
            feed.close(error.clone());
            if *error != SessionError::Cancelled {
                self.events.push(SessionEvent::Failed(error.clone()));
            }
            let _ = self.stop();
        }
        result
    }

    fn drive_buffered(&mut self, feed: &BufferedFeed, cancellation: &Cancellation) -> Result<BufferedProgress, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        let SessionProfile::Buffered(format) = &self.options.profile else {
            return Err(SessionError::UnsupportedProfile);
        };
        let client = self.client.as_mut().ok_or(SessionError::ConnectionLost)?;
        if !client.has_buffered() {
            return Err(SessionError::UnsupportedProfile);
        }
        let guard = Deadline::new(self.socket.clone(), self.options.connection_timeout, cancellation.clone());
        client.deadline = Some((guard.end, cancellation.clone()));
        let armed = crate::client::arm_buffered_synchronized(std::slice::from_mut(client));
        client.deadline = None;
        let armed = guard.check().and(armed.map_err(translate));
        drop(guard);
        let anchor_local = armed?;
        let events = &mut self.events;
        let mut on_progress = |p: BufferedProgress| match events.last_mut() {
            Some(SessionEvent::Buffered(last)) => *last = p,
            _ => events.push(SessionEvent::Buffered(p)),
        };
        let cancelled = || cancellation.is_cancelled();
        crate::client::stream_buffered_armed(std::slice::from_mut(client), format, anchor_local, feed, &cancelled, &mut on_progress)
            .map_err(|e| if cancellation.is_cancelled() { SessionError::Cancelled } else { translate(e) })
    }

    pub fn profile(&self) -> Option<ProfileSelection> {
        self.selection
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
        if self.realtime.is_some() && !self.armed {
            crate::client::begin_live_realtime_synchronized(std::slice::from_mut(client)).map_err(translate)?;
            self.armed = true;
            Ok(())
        } else if client.has_buffered() {
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

struct ClientTarget<'a> {
    client: &'a mut Client,
    route: &'a Route,
}

impl ProfileTarget for ClientTarget<'_> {
    fn route(&self) -> &Route {
        self.route
    }
    fn setup(&mut self, profile: &SessionProfile) -> Result<(), SessionError> {
        match profile {
            SessionProfile::Realtime { sample_rate } => self.client.setup_realtime(*sample_rate, 2),
            SessionProfile::Buffered(format) => self.client.setup_buffered(format),
        }
        .map_err(translate)
    }
}

fn translate(error: anyhow::Error) -> SessionError {
    if let Some(typed) = error.downcast_ref::<SessionError>() {
        return typed.clone();
    }
    if let Some(pairing) = error.downcast_ref::<crate::pairing::PairingError>() {
        use crate::pairing::PairingError as P;
        return match pairing {
            P::PasswordRequired => SessionError::PasswordRequired,
            P::Backoff { retry_after } => SessionError::RetryAfter { seconds: retry_after.map(|d| d.as_secs()) },
            P::AttemptsExhausted => SessionError::RetryAfter { seconds: None },
            P::Unavailable => SessionError::ReceiverUnavailable,
            P::Cancelled => SessionError::Cancelled,
            P::TimedOut => SessionError::Timeout,
            p if p.requires_re_pair() => SessionError::ChallengeRequired,
            _ => SessionError::Authentication,
        };
    }
    if error.chain().any(|c| c.is::<StoreError>()) {
        return SessionError::LocalPermissions;
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
            selection: None,
            realtime: None,
            armed: false,
        };
        assert_eq!(session.write_realtime_samples(&[0.0; 2]), Err(SessionError::UnsupportedProfile));
        let start = Instant::now();
        session.cancel().unwrap();
        session.stop().unwrap();
        assert!(start.elapsed() < session.options.shutdown_timeout);
        assert_eq!(session.events().collect::<Vec<_>>(), vec![SessionEvent::Stopped]);
        assert_eq!(std::io::Read::read(&mut receiver, &mut [0]).unwrap(), 0);
    }

    #[test]
    fn buffered_source_requires_a_24_bit_48k_buffered_profile_and_failures_close_the_feed() {
        let (mut session, _data) = realtime_session();
        assert!(matches!(session.buffered_source(), Err(SessionError::UnsupportedProfile)));
        for format in [BufferedAudioFormat::pcm_48000_24_stereo(true), BufferedAudioFormat::alac_48000_24_stereo()] {
            session.options.profile = SessionProfile::Buffered(format);
            assert!(session.buffered_source().is_ok());
        }
        let (producer, feed) = session.buffered_source().unwrap();
        session.client = Some(Client::new());
        assert_eq!(session.stream_buffered(&feed), Err(SessionError::UnsupportedProfile));
        assert_eq!(producer.push(&[0.0; 2]), Err(SessionError::UnsupportedProfile));
        assert_eq!(session.events().collect::<Vec<_>>(), vec![SessionEvent::Failed(SessionError::UnsupportedProfile), SessionEvent::Stopped]);
        session.options.profile = SessionProfile::Buffered(BufferedAudioFormat::aac_48000_stereo());
        assert!(matches!(session.buffered_source(), Err(SessionError::UnsupportedProfile)));
    }

    fn realtime_session() -> (Session, std::net::UdpSocket) {
        let data = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut stream = crate::stream::RealtimeStream::new(&[1; 32], "127.0.0.1", 48000, 352, 9).unwrap();
        stream.start(data.local_addr().unwrap().port() as i64, 0).unwrap();
        let latency = stream.latency();
        let mut client = Client::new();
        client.attach_realtime(stream);
        let options = SessionOptions { profile: SessionProfile::Realtime { sample_rate: 48000 }, ..Default::default() };
        let session = Session {
            client: Some(client),
            options,
            socket: Arc::new(Mutex::new(None)),
            events: Vec::new(),
            selection: None,
            realtime: Some(RealtimeSender::new(48000, latency)),
            armed: true,
        };
        (session, data)
    }

    #[test]
    fn realtime_samples_report_progress_and_cancel_within_deadline() {
        let (mut session, _data) = realtime_session();
        assert_eq!(session.realtime_output(), Some(OutputFormat { sample_rate: 48000, bits: 16 }));
        let progress = session.write_realtime_samples(&[0.25; 352 * 2 + 10]).unwrap();
        assert_eq!((progress.packets, progress.frames), (1, 352));
        assert_eq!(session.finish_realtime().unwrap().packets, 2);
        assert_eq!(session.write_realtime_samples(&[0.0; 3]), Err(SessionError::InvalidInput));
        let token = session.options.cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            token.cancel();
        });
        let start = Instant::now();
        assert_eq!(session.write_realtime_samples(&vec![0.0; 48000 * 20]), Err(SessionError::Cancelled));
        assert!(start.elapsed() < Duration::from_millis(300));
        canceller.join().unwrap();
        assert_eq!(session.events().collect::<Vec<_>>(), vec![SessionEvent::Stopped]);
        assert_eq!(session.write_realtime_samples(&[0.0; 2]), Err(SessionError::Cancelled));
    }

    #[test]
    fn realtime_receiver_disappearance_fails_and_stops_the_session() {
        let (mut session, data) = realtime_session();
        drop(data);
        let start = Instant::now();
        assert_eq!(session.write_realtime_samples(&vec![0.0; 48000 * 20]), Err(SessionError::ConnectionLost));
        assert!(start.elapsed() < Duration::from_millis(500));
        let events: Vec<_> = session.events().collect();
        assert_eq!(events, vec![SessionEvent::Failed(SessionError::ConnectionLost), SessionEvent::Stopped]);
        assert_eq!(session.write_realtime_samples(&[0.0; 2]), Err(SessionError::ConnectionLost));
    }
}
