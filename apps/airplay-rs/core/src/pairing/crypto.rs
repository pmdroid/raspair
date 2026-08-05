//! Crypto primitives shared by the pairing handshakes: HKDF-SHA512,
//! Curve25519 ECDH, and ChaCha20-Poly1305 AEAD.

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha512;
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

/// HKDF-Expand (extract+expand) over SHA-512, matching Go's `hkdf.Key`.
pub fn hkdf_expand(secret: &[u8], salt: &[u8], info: &str, length: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha512>::new(Some(salt), secret);
    let mut okm = vec![0u8; length];
    hk.expand(info.as_bytes(), &mut okm)
        .expect("hkdf length within bounds");
    okm
}

/// Right-align a short label (e.g. "PS-Msg05") in a zero-padded 12-byte nonce.
pub fn nonce_from_string(label: &str) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    let l = label.as_bytes();
    nonce[12 - l.len()..].copy_from_slice(l);
    nonce
}

pub fn generate_curve25519_keypair() -> ([u8; 32], [u8; 32]) {
    let mut private = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut private);
    let public = x25519(private, X25519_BASEPOINT_BYTES);
    (private, public)
}

pub fn shared_secret(private: &[u8; 32], public: &[u8]) -> Result<[u8; 32]> {
    let pub_arr: [u8; 32] = public
        .try_into()
        .map_err(|_| anyhow!("peer public key must be 32 bytes"))?;
    Ok(x25519(*private, pub_arr))
}

pub fn chacha20poly1305_encrypt(key: &[u8], nonce: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: plaintext, aad })
        .map_err(|e| anyhow!("chacha20poly1305 encrypt: {e}"))
}

pub fn chacha20poly1305_decrypt(key: &[u8], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|e| anyhow!("chacha20poly1305 decrypt: {e}"))
}
