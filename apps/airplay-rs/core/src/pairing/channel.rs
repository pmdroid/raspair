//! AirPlay 2 control/event channel cipher (matches pair_ap). After
//! pair-verify (or transient pair-setup) every byte on the connection is a
//! block of: `u16 LE plaintext-len || ciphertext || 16-byte Poly1305 tag`.
//! AAD is the 2-byte length prefix; the nonce is 4 zero bytes followed by a
//! per-direction u64 LE block counter from 0. Plaintext is capped at 0x400.

use super::crypto::hkdf_expand;
use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::io::Read;

const CHANNEL_BLOCK_MAX: usize = 0x400;

struct Direction {
    key: Vec<u8>,
    counter: u64,
}

impl Direction {
    fn nonce(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_le_bytes());
        n
    }
}

pub struct ChannelCipher {
    enc: Direction,
    dec: Direction,
}

impl ChannelCipher {
    /// Control-channel keys from the pair-verify shared secret (or the SRP
    /// session key for transient pairing).
    pub fn new_control(shared_secret: &[u8]) -> Self {
        Self::new(
            shared_secret,
            "Control-Salt",
            "Control-Write-Encryption-Key",
            "Control-Read-Encryption-Key",
        )
    }

    /// Event-channel keys. The event connection is a reverse channel, so our
    /// write direction uses the receiver's read key.
    pub fn new_event(shared_secret: &[u8]) -> Self {
        Self::new(
            shared_secret,
            "Events-Salt",
            "Events-Read-Encryption-Key",
            "Events-Write-Encryption-Key",
        )
    }

    fn new(shared_secret: &[u8], salt: &str, write_info: &str, read_info: &str) -> Self {
        Self {
            enc: Direction {
                key: hkdf_expand(shared_secret, salt.as_bytes(), write_info, 32),
                counter: 0,
            },
            dec: Direction {
                key: hkdf_expand(shared_secret, salt.as_bytes(), read_info, 32),
                counter: 0,
            },
        }
    }

    /// Frame and encrypt an outgoing message into one or more blocks.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.is_empty() {
            bail!("empty plaintext");
        }
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.enc.key));
        let mut out = Vec::new();
        let mut off = 0;
        while off < plaintext.len() {
            let n = (plaintext.len() - off).min(CHANNEL_BLOCK_MAX);
            let len_prefix = (n as u16).to_le_bytes();
            out.extend_from_slice(&len_prefix);
            let ct = cipher
                .encrypt(
                    Nonce::from_slice(&self.enc.nonce()),
                    Payload {
                        msg: &plaintext[off..off + n],
                        aad: &len_prefix,
                    },
                )
                .map_err(|e| anyhow!("channel encrypt: {e}"))?;
            out.extend_from_slice(&ct);
            self.enc.counter += 1;
            off += n;
        }
        Ok(out)
    }

    /// Read and decrypt exactly one block from `r`.
    pub fn read_block<R: Read>(&mut self, r: &mut R) -> Result<Vec<u8>> {
        let mut len_prefix = [0u8; 2];
        r.read_exact(&mut len_prefix)?;
        let n = u16::from_le_bytes(len_prefix) as usize;
        if n > CHANNEL_BLOCK_MAX {
            bail!("encrypted block too large: {n}");
        }
        let mut block = vec![0u8; n + 16];
        r.read_exact(&mut block)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.dec.key));
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&self.dec.nonce()),
                Payload {
                    msg: &block,
                    aad: &len_prefix,
                },
            )
            .map_err(|e| anyhow!("channel decrypt (block {}): {e}", self.dec.counter))?;
        self.dec.counter += 1;
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_multi_block() {
        let secret = [0x42u8; 32];
        let mut sender = ChannelCipher::new_control(&secret);
        let mut receiver = ChannelCipher::new_control(&secret);
        // The receiver decrypts what the sender encrypts: swap directions.
        std::mem::swap(&mut receiver.enc, &mut receiver.dec);

        let big: Vec<u8> = b"0123456789abcdef".repeat(200); // 3200 bytes -> 4 blocks
        let small = b"RTSP/1.0 200 OK\r\n\r\n".to_vec();

        let mut wire = Vec::new();
        wire.extend_from_slice(&sender.encrypt(&big).unwrap());
        wire.extend_from_slice(&sender.encrypt(&small).unwrap());

        let mut cur = Cursor::new(wire);
        let mut got = Vec::new();
        while got.len() < big.len() + small.len() {
            got.extend_from_slice(&receiver.read_block(&mut cur).unwrap());
        }

        let mut want = big.clone();
        want.extend_from_slice(&small);
        assert_eq!(got, want);
    }
}
