use crate::profile::{self, FallbackPolicy, ProfileSelection, ProfileTarget};
use crate::audio::convert::OutputFormat;
use crate::pairing::{CredentialStore, StoreError};
use crate::protocol::Route;
use crate::stream::{BufferedCodec, BufferedFeed, BufferedProducer, BufferedProgress, RealtimeProgress, RealtimeSender};
use crate::client::{MemberFault, Members};
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
    #[error("binding PTP timing ports UDP 319 and 320 was denied; grant the player CAP_NET_BIND_SERVICE (setcap cap_net_bind_service=+ep) instead of running as root")]
    PtpPermission,
    #[error("stereo member {member} failed: {error}")]
    MemberFailed { member: String, error: Box<SessionError> },
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
    GroupReady { group: String, left: String, right: String },
    MemberFailed { member: String, error: SessionError },
    Profile(ProfileSelection),
    Buffered(BufferedProgress),
    Stopped,
    Failed(SessionError),
}

#[derive(Debug, Clone)]
pub struct StereoPair {
    pub group: String,
    pub left: Device,
    pub right: Device,
}

impl StereoPair {
    pub fn resolve(devices: &[Device], selected: &Device) -> Option<Self> {
        let group = selected.tight_sync_id();
        if group.is_empty() || selected.record.device_id.is_empty() {
            return None;
        }
        let mut mates = devices
            .iter()
            .filter(|d| d.tight_sync_id() == group && !d.record.device_id.is_empty() && d.record.device_id != selected.record.device_id);
        let mate = mates.next()?;
        if mates.any(|d| d.record.device_id != mate.record.device_id) {
            return None;
        }
        let (left, right) = if selected.record.device_id < mate.record.device_id { (selected, mate) } else { (mate, selected) };
        Some(Self { group: group.to_string(), left: left.clone(), right: right.clone() })
    }
}

type Sockets = Arc<Mutex<Vec<TcpStream>>>;

pub struct Session {
    clients: Vec<Client>,
    options: SessionOptions,
    socket: Sockets,
    events: Vec<SessionEvent>,
    selection: Option<ProfileSelection>,
    realtime: Option<RealtimeSender>,
    armed: bool,
}

impl Session {
    pub fn connect(device: &Device, pin: &str, options: SessionOptions) -> Result<Self, SessionError> {
        Self::connect_member(device, pin, options, None)
    }

    pub fn connect_pair(pair: &StereoPair, pin: &str, options: SessionOptions) -> Result<Self, SessionError> {
        let route = pair.right.route();
        route.hap_pairing().and_then(|_| profile::preflight(&route, &options.profile, &options.fallback)).map_err(|e| member_failed(&pair.right, e))?;
        Self::assemble_pair(pair, options, |device, options, leader| Self::connect_member(device, pin, options.clone(), leader))
    }

    fn assemble_pair(
        pair: &StereoPair,
        options: SessionOptions,
        mut open: impl FnMut(&Device, &SessionOptions, Option<&Client>) -> Result<Self, SessionError>,
    ) -> Result<Self, SessionError> {
        let mut session = open(&pair.left, &options, None).map_err(|e| member_failed(&pair.left, e))?;
        session.clients[0].member = Some(pair.left.record.device_id.clone());
        let follower = SessionOptions { profile: session.options.profile.clone(), fallback: FallbackPolicy::Forbid, ..options };
        let mut right = match open(&pair.right, &follower, session.clients.first()) {
            Ok(right) => right,
            Err(error) => {
                let _ = session.stop();
                return Err(member_failed(&pair.right, error));
            }
        };
        right.clients[0].member = Some(pair.right.record.device_id.clone());
        session.clients.append(&mut right.clients);
        session.socket.lock().unwrap().append(&mut right.socket.lock().unwrap());
        session.events = vec![
            SessionEvent::Connected,
            SessionEvent::GroupReady {
                group: pair.group.clone(),
                left: pair.left.record.device_id.clone(),
                right: pair.right.record.device_id.clone(),
            },
        ];
        session.events.extend(session.selection.map(SessionEvent::Profile));
        Ok(session)
    }

    fn connect_member(device: &Device, pin: &str, options: SessionOptions, leader: Option<&Client>) -> Result<Self, SessionError> {
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
        let socket = Sockets::default();
        let guard = Deadline::new(socket.clone(), options.connection_timeout, options.cancellation.clone());
        let mut client = Client::new();
        client.set_startup_volume(options.volume_db);
        if let Some(store) = &options.credentials {
            client.set_credential_store(store.clone());
        }
        if let Some(password) = &options.password {
            client.set_password(password);
        }
        if let Some(leader) = leader {
            if let Some(ptp) = leader.ptp_clock() {
                client.set_ptp_clock(ptp);
            }
            client.set_group_uuid(leader.group_uuid());
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
            socket.lock().unwrap().push(stream.try_clone().map_err(|e| translate(e.into()))?);
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
                for s in socket.lock().unwrap().drain(..) {
                    let _ = s.shutdown(Shutdown::Both);
                }
                let _ = client.stop();
                return Err(error);
            }
        };
        client.deadline = None;
        for s in socket.lock().unwrap().iter() {
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
        Ok(Self { clients: vec![client], options, socket, events, selection: Some(selection), realtime, armed: false })
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
        let mut members = Members(connected(&mut self.clients)?);
        let cancelled = || cancellation.is_cancelled();
        let mut result = Ok(());
        if !self.armed {
            result = crate::client::begin_live_realtime_synchronized(members.0);
            self.armed = result.is_ok();
        }
        let result = result
            .and_then(|()| match samples {
                Some(samples) => sender.push(&mut members, samples, &cancelled),
                None => sender.finish(&mut members, &cancelled),
            })
            .map_err(translate);
        if let Err(ref error) = result {
            if *error != SessionError::InvalidInput {
                if *error != SessionError::Cancelled {
                    self.record_failure(error);
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
        self.stream_buffered_controlled(feed, &|| false, &mut || None)
    }

    pub fn stream_buffered_controlled(
        &mut self,
        feed: &BufferedFeed,
        interrupted: &dyn Fn() -> bool,
        volume: &mut dyn FnMut() -> Option<f64>,
    ) -> Result<BufferedProgress, SessionError> {
        let cancellation = self.options.cancellation.clone();
        let result = self.drive_buffered(feed, &cancellation, interrupted, volume);
        if result.is_err() && interrupted() && !cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        if let Err(ref error) = result {
            feed.close(error.clone());
            if *error != SessionError::Cancelled {
                self.record_failure(error);
            }
            let _ = self.stop();
        }
        result
    }

    fn drive_buffered(
        &mut self,
        feed: &BufferedFeed,
        cancellation: &Cancellation,
        interrupted: &dyn Fn() -> bool,
        volume: &mut dyn FnMut() -> Option<f64>,
    ) -> Result<BufferedProgress, SessionError> {
        if cancellation.is_cancelled() {
            return Err(SessionError::Cancelled);
        }
        let SessionProfile::Buffered(format) = &self.options.profile else {
            return Err(SessionError::UnsupportedProfile);
        };
        let clients = connected(&mut self.clients)?;
        if !clients.iter().all(Client::has_buffered) {
            return Err(SessionError::UnsupportedProfile);
        }
        let guard = Deadline::new(self.socket.clone(), self.options.connection_timeout, cancellation.clone());
        for client in clients.iter_mut() {
            client.deadline = Some((guard.end, cancellation.clone()));
        }
        let armed = crate::client::arm_buffered_synchronized(clients);
        for client in clients.iter_mut() {
            client.deadline = None;
        }
        let armed = guard.check().and(armed.map_err(translate));
        drop(guard);
        let anchor_local = armed?;
        let events = &mut self.events;
        let mut on_progress = |p: BufferedProgress| match events.last_mut() {
            Some(SessionEvent::Buffered(last)) => *last = p,
            _ => events.push(SessionEvent::Buffered(p)),
        };
        let cancelled = || cancellation.is_cancelled();
        let mut control = |clients: &mut [Client]| match volume() {
            Some(db) => clients.iter_mut().try_for_each(|c| c.tagged(|c| c.set_volume(db))),
            None => Ok(()),
        };
        crate::client::stream_buffered_armed(clients, format, anchor_local, feed, &cancelled, interrupted, &mut on_progress, &mut control)
            .map_err(|e| if cancellation.is_cancelled() { SessionError::Cancelled } else { translate(e) })
    }

    pub fn startup_volume_db(&self) -> f64 {
        self.options.volume_db
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
        let realtime = matches!(self.options.profile, SessionProfile::Realtime { .. });
        let result = connected(&mut self.clients)?
            .iter_mut()
            .try_for_each(|client| {
                client.tagged(|c| if realtime { c.write_realtime_frame(payload, marker) } else { c.write_buffered_frame(payload) })
            })
            .map_err(translate);
        if let Err(ref e) = result {
            self.record_failure(e);
            if self.clients.len() > 1 {
                let _ = self.stop();
            }
        }
        result
    }

    pub fn begin_playback(&mut self) -> Result<(), SessionError> {
        let result = match &mut self.clients[..] {
            [] => Err(SessionError::ConnectionLost),
            clients if self.realtime.is_some() && !self.armed => {
                let armed = crate::client::begin_live_realtime_synchronized(clients).map_err(translate);
                self.armed = armed.is_ok();
                armed
            }
            [client] if client.has_buffered() => client.begin_buffered_playback().map_err(translate),
            clients if clients.iter().all(Client::has_buffered) => {
                crate::client::begin_live_buffered_synchronized(clients).map_err(translate)
            }
            _ => Ok(()),
        };
        if let (Err(error), true) = (&result, self.clients.len() > 1) {
            self.record_failure(&error.clone());
            let _ = self.stop();
        }
        result
    }

    pub fn set_volume(&mut self, db: f64) -> Result<(), SessionError> {
        if !db.is_finite() {
            return Err(SessionError::InvalidInput);
        }
        connected(&mut self.clients)?.iter_mut().try_for_each(|client| client.tagged(|c| c.set_volume(db))).map_err(translate)
    }

    pub fn flush_buffered(&mut self) -> Result<(), SessionError> {
        let result = connected(&mut self.clients)?.iter_mut().try_for_each(|c| c.tagged(Client::flush_buffered)).map_err(translate);
        if let Err(ref error) = result {
            self.record_failure(error);
            let _ = self.stop();
        }
        result
    }

    fn record_failure(&mut self, error: &SessionError) {
        if let SessionError::MemberFailed { member, error } = error {
            self.events.push(SessionEvent::MemberFailed { member: member.clone(), error: (**error).clone() });
        }
        self.events.push(SessionEvent::Failed(error.clone()));
    }

    pub fn cancel(&mut self) -> Result<(), SessionError> {
        self.options.cancellation.cancel();
        self.stop()
    }

    pub fn stop(&mut self) -> Result<(), SessionError> {
        if self.clients.is_empty() {
            return Ok(());
        }
        let mut clients = std::mem::take(&mut self.clients);
        let guard = Deadline::new(self.socket.clone(), self.options.shutdown_timeout, Cancellation::default());
        let result = clients.iter_mut().map(|client| client.stop().map_err(translate)).fold(Ok(()), Result::and);
        drop(clients);
        for s in self.socket.lock().unwrap().drain(..) {
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
    fn new(socket: Sockets, timeout: Duration, cancellation: Cancellation) -> Self {
        let end = Instant::now() + timeout;
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let cancel = cancellation.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if cancel.is_cancelled() || Instant::now() >= end {
                    for s in socket.lock().unwrap().iter() {
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

fn connected(clients: &mut [Client]) -> Result<&mut [Client], SessionError> {
    if clients.is_empty() {
        return Err(SessionError::ConnectionLost);
    }
    Ok(clients)
}

fn member_failed(device: &Device, error: SessionError) -> SessionError {
    match error {
        SessionError::MemberFailed { .. } => error,
        error => SessionError::MemberFailed { member: device.record.device_id.clone(), error: Box::new(error) },
    }
}

fn translate(error: anyhow::Error) -> SessionError {
    match error.downcast_ref::<MemberFault>() {
        Some(MemberFault(member)) => SessionError::MemberFailed { member: member.clone(), error: Box::new(classify(error)) },
        None => classify(error),
    }
}

fn classify(error: anyhow::Error) -> SessionError {
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
            clients: vec![Client::new()],
            options: SessionOptions::default(),
            socket: Arc::new(Mutex::new(vec![stream])),
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
        session.clients = vec![Client::new()];
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
            clients: vec![client],
            options,
            socket: Sockets::default(),
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

    const KEY: [u8; 32] = [7; 32];

    struct FakeReceiver {
        requests: std::thread::JoinHandle<Vec<(String, Vec<u8>)>>,
        data: std::net::TcpListener,
    }

    fn device(id: &str) -> Device {
        let record = crate::protocol::TxtRecord { device_id: id.into(), tight_sync_id: "pair".into(), ..Default::default() };
        Device { name: id.into(), host: id.into(), port: 7000, ipv4: None, ipv6: None, record, last_seen: 0 }
    }

    fn member(ptp: Arc<crate::PtpClock>) -> (Client, FakeReceiver) {
        use std::io::{BufRead, Write};
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(control.local_addr().unwrap()).unwrap();
        let (socket, _) = control.accept().unwrap();
        let requests = std::thread::spawn(move || {
            let mut writer = socket.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(socket);
            let mut seen = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return seen;
                }
                let (mut cseq, mut len) = (String::new(), 0);
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    match header.split_once(':').map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())) {
                        Some((k, v)) if k == "cseq" => cseq = v,
                        Some((k, v)) if k == "content-length" => len = v.parse().unwrap(),
                        _ => {}
                    }
                    header.clear();
                }
                let mut body = vec![0; len];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                seen.push((line.split(' ').next().unwrap().to_string(), body));
                let _ = writer.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n").as_bytes());
            }
        });
        let rtsp = crate::rtsp::Client::from_stream("127.0.0.1", 7000, stream).unwrap();
        let port = data.local_addr().unwrap().port() as i64;
        let mut buffered = crate::stream::BufferedStream::new(&KEY, "127.0.0.1", port, 0, 48000, 352, crate::stream::buffered::SSRC_BUFFERED_PCM48000_24_STEREO);
        buffered.start().unwrap();
        let mut client = Client::new();
        client.attach_buffered(rtsp, buffered, ptp);
        (client, FakeReceiver { requests, data })
    }

    fn read_packet(sock: &mut TcpStream) -> (u32, u32, Vec<u8>) {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use std::io::Read;
        let mut len = [0u8; 2];
        sock.read_exact(&mut len).unwrap();
        let mut packet = vec![0u8; u16::from_be_bytes(len) as usize - 2];
        sock.read_exact(&mut packet).unwrap();
        let (ct, tail) = packet[12..].split_at(packet.len() - 20);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(tail);
        let cipher = chacha20poly1305::ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&KEY));
        let plain = cipher.decrypt(chacha20poly1305::Nonce::from_slice(&nonce), Payload { msg: ct, aad: &packet[4..12] }).unwrap();
        (u32::from_be_bytes([0, packet[1], packet[2], packet[3]]), u32::from_be_bytes(packet[4..8].try_into().unwrap()), plain)
    }

    fn connect_fake_pair(
        clock: &Arc<crate::PtpClock>,
        right: impl FnOnce() -> Result<(), SessionError>,
    ) -> (Result<Session, SessionError>, Vec<FakeReceiver>) {
        let pair = StereoPair::resolve(&[device("bb"), device("aa")], &device("bb")).unwrap();
        let mut fakes = Vec::new();
        let mut right = Some(right);
        let session = Session::assemble_pair(&pair, SessionOptions::default(), |device, options, leader| {
            if device.record.device_id == "bb" {
                (right.take().unwrap())()?;
            }
            let (client, fake) = member(leader.and_then(Client::ptp_clock).unwrap_or_else(|| clock.clone()));
            fakes.push(fake);
            let output = crate::profile::OutputProfile::of(&options.profile)?;
            let selection = Some(ProfileSelection { requested: output, effective: output, fallback: None });
            Ok(Session { clients: vec![client], options: options.clone(), socket: Sockets::default(), events: Vec::new(), selection, realtime: None, armed: false })
        });
        (session, fakes)
    }

    fn methods(requests: &[(String, Vec<u8>)]) -> Vec<&str> {
        requests.iter().map(|(m, _)| m.as_str()).collect()
    }

    #[test]
    fn stereo_pair_resolution_is_stable_and_requires_exactly_one_mate() {
        let devices = [device("bb"), device("aa"), device("bb")];
        for selected in [&devices[0], &devices[1]] {
            let pair = StereoPair::resolve(&devices, selected).unwrap();
            assert_eq!((pair.group.as_str(), pair.left.record.device_id.as_str(), pair.right.record.device_id.as_str()), ("pair", "aa", "bb"));
        }
        assert!(StereoPair::resolve(&devices[..1], &devices[0]).is_none());
        assert!(StereoPair::resolve(&[device("aa"), device("bb"), device("cc")], &devices[1]).is_none());
        let mut single = device("aa");
        single.record.tight_sync_id.clear();
        assert!(StereoPair::resolve(&devices, &single).is_none());
    }

    #[test]
    fn pair_members_share_one_anchor_and_identical_stereo_frames_and_stop_together() {
        for fail_right in [false, true] {
            let clock = Arc::new(crate::PtpClock::locked(0xfeed));
            let (session, fakes) = connect_fake_pair(&clock, || Ok(()));
            let mut session = session.unwrap();
            let ready = SessionEvent::GroupReady { group: "pair".into(), left: "aa".into(), right: "bb".into() };
            let events: Vec<_> = session.events().collect();
            assert!(matches!(&events[..], [SessionEvent::Connected, r, SessionEvent::Profile(_)] if *r == ready));
            session.begin_playback().unwrap();
            let block: Vec<f32> = (0..352).flat_map(|i| [i as f32 / 1000.0, -(i as f32) / 1000.0]).collect();
            let payload = crate::stream::BufferedCodec::of(&BufferedAudioFormat::pcm_48000_24_stereo(true)).unwrap().encode(&block, 352);
            assert_eq!(payload.len(), 352 * 2 * 3);
            session.write_frame(&payload, false).unwrap();
            let mut data: Vec<_> = fakes.iter().map(|f| f.data.accept().unwrap().0).collect();
            let packets: Vec<_> = data.iter_mut().map(read_packet).collect();
            assert_eq!(packets[0], packets[1]);
            assert_eq!(packets[0].2, payload);
            if fail_right {
                drop(data.pop());
                let error = (0..200).find_map(|_| session.write_frame(&payload, false).err()).unwrap();
                assert!(matches!(&error, SessionError::MemberFailed { member, .. } if member == "bb"));
                let events: Vec<_> = session.events().collect();
                assert!(matches!(&events[0], SessionEvent::MemberFailed { member, .. } if member == "bb"));
                assert_eq!(events[1..], [SessionEvent::Failed(error), SessionEvent::Stopped]);
            } else {
                session.stop().unwrap();
            }
            let seen: Vec<_> = fakes.into_iter().map(|f| f.requests.join().unwrap()).collect();
            for requests in &seen {
                assert_eq!(methods(requests), ["RECORD", "FLUSHBUFFERED", "SETRATEANCHORTIME", "TEARDOWN"]);
            }
            assert_eq!(seen[0][..3], seen[1][..3]);
            let anchor = plist::Value::from_reader(std::io::Cursor::new(&seen[0][2].1)).unwrap();
            assert_eq!(anchor.as_dictionary().unwrap()["networkTimeTimelineID"].as_unsigned_integer(), Some(0xfeed));
            drop(session);
            assert_eq!((Arc::strong_count(&clock), clock.workers()), (1, 0));
        }
    }

    #[test]
    fn delayed_second_member_failure_releases_the_first_before_any_frame() {
        let clock = Arc::new(crate::PtpClock::locked(1));
        let (session, fakes) = connect_fake_pair(&clock, || {
            std::thread::sleep(Duration::from_millis(50));
            Err(SessionError::Timeout)
        });
        assert_eq!(session.err(), Some(SessionError::MemberFailed { member: "bb".into(), error: Box::new(SessionError::Timeout) }));
        let [left] = <[FakeReceiver; 1]>::try_from(fakes).ok().unwrap();
        let (mut data, _) = left.data.accept().unwrap();
        assert_eq!(std::io::Read::read(&mut data, &mut [0; 1]).unwrap(), 0);
        assert_eq!(methods(&left.requests.join().unwrap()), ["TEARDOWN"]);
        assert_eq!((Arc::strong_count(&clock), clock.workers()), (1, 0));
    }

    #[test]
    fn ptp_permission_errors_name_the_ports_and_capability() {
        let denied = crate::timing::ptp::bind_error(std::io::ErrorKind::PermissionDenied.into(), 319);
        let error = translate(denied);
        assert_eq!(error, SessionError::PtpPermission);
        let message = error.to_string();
        assert!(message.contains("UDP 319 and 320") && message.contains("CAP_NET_BIND_SERVICE"));
    }
}
