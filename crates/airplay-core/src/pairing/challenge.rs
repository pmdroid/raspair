use super::legacy::LegacyPairing;
use super::session::{PairTransport, PairingError, PairingSession};
use super::store::CredentialStore;
use num_bigint::BigUint;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const DEFAULT_CHALLENGE_EXPIRY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    OnScreen,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinChallenge {
    pub id: String,
    pub receiver_id: String,
    pub source: PinSource,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinPairingError {
    #[error("wrong pairing code; start a new attempt")]
    WrongCode,
    #[error("pairing challenge expired; start a new attempt")]
    Expired,
    #[error("receiver is backing off pairing attempts; wait and retry")]
    Backoff,
    #[error("receiver cannot pair another controller")]
    MaxPeers,
    #[error("pairing challenge cancelled")]
    Cancelled,
    #[error("pairing challenge already answered")]
    Finished,
    #[error("unknown pairing challenge")]
    UnknownChallenge,
    #[error("pairing failed: {0}")]
    Failed(String),
}

impl PinPairingError {
    fn classify(error: anyhow::Error) -> Self {
        match error.downcast_ref::<PairingError>() {
            Some(PairingError::Protocol { code: Some(0x02), .. }) => Self::WrongCode,
            Some(
                PairingError::Protocol { code: Some(0x03 | 0x05), .. }
                | PairingError::Backoff { .. }
                | PairingError::AttemptsExhausted,
            ) => Self::Backoff,
            Some(PairingError::Protocol { code: Some(0x04), .. }) => Self::MaxPeers,
            _ => Self::Failed(format!("{error:#}")),
        }
    }
}

enum Exchange {
    Hap { session: Box<PairingSession>, salt: Vec<u8>, b: BigUint },
    Legacy(Box<LegacyPairing>),
}

struct Pending {
    receiver_id: String,
    exchange: Exchange,
    transport: Box<dyn PairTransport + Send>,
    expires_at: Instant,
}

enum Slot {
    Pending(Box<Pending>),
    Closed(PinPairingError, Instant),
}

#[derive(Default)]
struct State {
    slots: HashMap<String, Slot>,
    current: HashMap<String, String>,
}

impl State {
    fn close(&mut self, id: &str, reason: PinPairingError) {
        self.slots.insert(id.to_string(), Slot::Closed(reason, Instant::now()));
    }

    fn release(&mut self, id: &str) -> bool {
        let before = self.current.len();
        self.current.retain(|_, current| current != id);
        self.current.len() != before
    }

    fn prune(&mut self, now: Instant) {
        let expired: Vec<String> = self
            .slots
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Pending(p) if p.expires_at <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.release(&id);
            self.close(&id, PinPairingError::Expired);
        }
        self.slots
            .retain(|_, slot| !matches!(slot, Slot::Closed(_, at) if now.duration_since(*at) >= DEFAULT_CHALLENGE_EXPIRY));
    }
}

pub struct PinPairings {
    store: Arc<dyn CredentialStore>,
    expiry: Duration,
    state: Mutex<State>,
}

impl PinPairings {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self { store, expiry: DEFAULT_CHALLENGE_EXPIRY, state: Mutex::default() }
    }

    pub fn with_expiry(mut self, expiry: Duration) -> Self {
        self.expiry = expiry;
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(Instant::now());
        state
    }

    pub fn begin(
        &self,
        receiver_id: &str,
        source: PinSource,
        transport: Box<dyn PairTransport + Send>,
    ) -> Result<PinChallenge, PinPairingError> {
        self.start(receiver_id, source, transport, false)
    }

    pub fn begin_legacy(
        &self,
        receiver_id: &str,
        source: PinSource,
        transport: Box<dyn PairTransport + Send>,
    ) -> Result<PinChallenge, PinPairingError> {
        self.start(receiver_id, source, transport, true)
    }

    fn start(
        &self,
        receiver_id: &str,
        source: PinSource,
        mut transport: Box<dyn PairTransport + Send>,
        legacy: bool,
    ) -> Result<PinChallenge, PinPairingError> {
        let id = format!("{:032x}", rand::random::<u128>());
        {
            let mut state = self.state();
            if let Some(previous) = state.current.insert(receiver_id.to_string(), id.clone()) {
                state.close(&previous, PinPairingError::Cancelled);
            }
        }
        let mut session = PairingSession::new(receiver_id, "");
        let started = session
            .load_or_create_identity(&*self.store)
            .and_then(|_| match legacy {
                true => LegacyPairing::begin(receiver_id, session.identity.clone().expect("identity loaded"), &mut *transport)
                    .map(|l| Exchange::Legacy(Box::new(l))),
                false => session.begin_persistent(&mut *transport).map(|(salt, b)| Exchange::Hap { session: Box::new(session), salt, b }),
            })
            .map_err(PinPairingError::classify);
        let mut state = self.state();
        let exchange = match started {
            Ok(started) if state.current.get(receiver_id) == Some(&id) => started,
            Ok(_) => return Err(PinPairingError::Cancelled),
            Err(e) => {
                state.release(&id);
                return Err(e);
            }
        };
        let expires_at = Instant::now() + self.expiry;
        let pending = Pending { receiver_id: receiver_id.to_string(), exchange, transport, expires_at };
        state.slots.insert(id.clone(), Slot::Pending(Box::new(pending)));
        Ok(PinChallenge { id, receiver_id: receiver_id.to_string(), source, expires_at })
    }

    pub fn respond(&self, id: &str, code: &str) -> Result<(), PinPairingError> {
        let mut pending = {
            let mut state = self.state();
            match state.slots.remove(id) {
                Some(Slot::Pending(pending)) => {
                    state.close(id, PinPairingError::Finished);
                    pending
                }
                Some(Slot::Closed(reason, at)) => {
                    state.slots.insert(id.to_string(), Slot::Closed(reason.clone(), at));
                    return Err(reason);
                }
                None => return Err(PinPairingError::UnknownChallenge),
            }
        };
        let transport = &mut *pending.transport;
        let finished = match &mut pending.exchange {
            Exchange::Hap { session, salt, b } => {
                session.finish_persistent(transport, salt, b.clone(), code).map(|_| session.pairing_record())
            }
            Exchange::Legacy(legacy) => legacy.finish(transport, code).map(Some),
        }
        .map_err(PinPairingError::classify)
        .and_then(|record| {
            record
                .filter(|record| record.receiver_id == pending.receiver_id)
                .ok_or_else(|| PinPairingError::Failed("pair-setup returned no accessory identity".into()))
        });
        let mut state = self.state();
        let current = state.release(id);
        let record = finished?;
        if !current {
            return Err(PinPairingError::Cancelled);
        }
        self.store.save(&record).map_err(|e| PinPairingError::Failed(format!("save pairing: {e}")))
    }

    pub fn cancel(&self, id: &str) -> Result<(), PinPairingError> {
        let mut state = self.state();
        let current = state.release(id);
        match state.slots.remove(id) {
            Some(Slot::Pending(_)) => {
                state.close(id, PinPairingError::Cancelled);
                Ok(())
            }
            Some(Slot::Closed(_, _)) if current => {
                state.close(id, PinPairingError::Cancelled);
                Ok(())
            }
            Some(Slot::Closed(reason, at)) => {
                state.slots.insert(id.to_string(), Slot::Closed(reason.clone(), at));
                Err(reason)
            }
            None => Err(PinPairingError::UnknownChallenge),
        }
    }
}
