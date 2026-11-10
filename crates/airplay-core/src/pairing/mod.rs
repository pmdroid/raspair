//! HomeKit/AirPlay pairing: TLV8, SRP-6a, Ed25519 identities, the
//! ChaCha20-Poly1305 channel cipher, and the pair-setup/pair-verify state
//! machine.

pub mod channel;
pub mod crypto;
#[cfg(test)]
mod fixtures;
pub mod identity;
pub mod session;
pub mod srp;
pub mod tlv;

pub use channel::ChannelCipher;
pub use identity::{identity_exists, Identity};
pub use session::{PairTransport, PairingSession};
