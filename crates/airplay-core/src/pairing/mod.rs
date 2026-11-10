//! HomeKit/AirPlay pairing: TLV8, SRP-6a, Ed25519 identities, the
//! ChaCha20-Poly1305 channel cipher, and the pair-setup/pair-verify state
//! machine.

pub mod challenge;
pub mod channel;
pub mod crypto;
#[cfg(test)]
mod fixtures;
pub mod identity;
pub mod legacy;
pub mod session;
pub mod srp;
pub mod store;
pub mod tlv;

pub use challenge::{PinChallenge, PinPairingError, PinPairings, PinSource, DEFAULT_CHALLENGE_EXPIRY};
pub use channel::ChannelCipher;
pub use identity::Identity;
pub use legacy::LegacyPairing;
pub use session::{PairTransport, PairingError, PairingSession};
pub use store::{
    resolve_state_dir, AuthMethod, CredentialStore, FileStore, MemoryStore, PairingRecord, StoreError, Stored,
};
