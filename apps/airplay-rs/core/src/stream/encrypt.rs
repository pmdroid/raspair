//! Per-packet ChaCha20-Poly1305 audio encryption and packet assembly for both
//! the realtime (type 96) and buffered (type 103) transports.
//!
//! AAD is `timestamp BE || ssrc BE`. The realtime nonce puts the 16-bit
//! sequence at bytes [4:6] (LE); the buffered nonce puts a 64-bit packet
//! counter at [4:12] (LE). The 8-byte tail (nonce[4:]) is appended after the
//! ciphertext+tag so the receiver can reconstruct the nonce.

use super::rtp::build_rtp_header;
use aes::cipher::{BlockEncrypt, KeyInit as AesKeyInit};
use aes::Aes128;
use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;

fn aad(timestamp: u32, ssrc: u32) -> [u8; 8] {
    let mut a = [0u8; 8];
    a[0..4].copy_from_slice(&timestamp.to_be_bytes());
    a[4..8].copy_from_slice(&ssrc.to_be_bytes());
    a
}

fn seal(key: &[u8], nonce: &[u8; 12], payload: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: payload, aad })
        .map_err(|e| anyhow!("audio encrypt: {e}"))?;
    // Append the 8-byte nonce tail (nonce[4:]) after ciphertext+tag.
    let mut out = ct;
    out.extend_from_slice(&nonce[4..]);
    Ok(out)
}

/// Realtime (type 96) encrypted payload, keyed by the 16-bit sequence number.
pub fn encrypt_realtime(key: &[u8], ssrc: u32, seq: u16, timestamp: u32, payload: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    nonce[4..6].copy_from_slice(&seq.to_le_bytes());
    seal(key, &nonce, payload, &aad(timestamp, ssrc))
}

/// Buffered (type 103) encrypted payload, keyed by a 64-bit packet counter.
pub fn encrypt_buffered(key: &[u8], ssrc: u32, nonce_counter: u64, timestamp: u32, payload: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    nonce[4..12].copy_from_slice(&nonce_counter.to_le_bytes());
    seal(key, &nonce, payload, &aad(timestamp, ssrc))
}

/// Full realtime RTP packet: 12-byte header + encrypted payload + nonce tail.
pub fn build_audio_packet(
    seq: u16,
    ts: u32,
    ssrc: u32,
    shk: &[u8],
    marker: bool,
    payload: &[u8],
    payload_type: i32,
) -> Result<Vec<u8>> {
    let enc = encrypt_realtime(shk, ssrc, seq, ts, payload)?;
    let header = build_rtp_header(seq, ts, ssrc, marker, payload_type);
    let mut packet = Vec::with_capacity(header.len() + enc.len());
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&enc);
    Ok(packet)
}

/// Full buffered packet: a 12-byte header carrying the 24-bit sequence and the
/// codec-tagged SSRC, then the encrypted payload + nonce tail.
pub fn build_buffered_audio_packet(
    seq: u32,
    ts: u32,
    ssrc: u32,
    key: &[u8],
    nonce_counter: u64,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let enc = encrypt_buffered(key, ssrc, nonce_counter, ts, payload)?;
    let mut packet = vec![0u8; 12 + enc.len()];
    let seq24 = seq & 0xFF_FFFF;
    packet[0] = 0x80;
    packet[1] = (seq24 >> 16) as u8;
    packet[2] = (seq24 >> 8) as u8;
    packet[3] = seq24 as u8;
    packet[4..8].copy_from_slice(&ts.to_be_bytes());
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
    packet[12..].copy_from_slice(&enc);
    Ok(packet)
}

/// FairPlay AES-128-CBC buffered packet. Used when the group leader negotiates
/// the legacy FairPlay audio path (`ekey/eiv/et` in SETUP, 16-byte `shk`).
/// Only full 16-byte blocks are encrypted; any trailing bytes are sent in the
/// clear, matching the Doubletake/AirPlay legacy AES behaviour.
pub fn build_buffered_audio_packet_aes(
    seq: u32,
    ts: u32,
    ssrc: u32,
    key: &[u8],
    iv: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>> {
    if key.len() != 16 {
        bail!("AES buffered key must be 16 bytes, got {}", key.len());
    }
    if iv.len() != 16 {
        bail!("AES buffered IV must be 16 bytes, got {}", iv.len());
    }

    let block_size = 16;
    let enc_len = (payload.len() / block_size) * block_size;
    let mut enc = payload.to_vec();
    if enc_len > 0 {
        let cipher = Aes128::new_from_slice(key)
            .map_err(|e| anyhow!("AES-128 init: {e:?}"))?;
        let mut prev = iv.to_vec();
        for chunk in enc.chunks_mut(block_size) {
            if chunk.len() != block_size {
                break;
            }
            for i in 0..block_size {
                chunk[i] ^= prev[i];
            }
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            cipher.encrypt_block((&mut block).into());
            chunk.copy_from_slice(&block);
            prev = block.to_vec();
        }
    }

    let mut packet = vec![0u8; 12 + enc.len()];
    let seq24 = seq & 0xFF_FFFF;
    packet[0] = 0x80;
    packet[1] = (seq24 >> 16) as u8;
    packet[2] = (seq24 >> 8) as u8;
    packet[3] = seq24 as u8;
    packet[4..8].copy_from_slice(&ts.to_be_bytes());
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
    packet[12..].copy_from_slice(&enc);
    Ok(packet)
}

pub fn generate_shk() -> Vec<u8> {
    let mut key = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecrypt;

    #[test]
    fn aes_cbc_full_block_roundtrip() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let payload = [3u8; 32]; // two full blocks
        let packet = build_buffered_audio_packet_aes(1, 2, 0x16000000, &key, &iv, &payload).unwrap();

        // Parse back: 12-byte header + encrypted payload
        let enc = &packet[12..];
        assert_eq!(enc.len(), payload.len());

        // Manually decrypt CBC to verify
        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut prev = iv.to_vec();
        let mut dec = vec![0u8; payload.len()];
        for (i, chunk) in enc.chunks(16).enumerate() {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            cipher.decrypt_block((&mut block).into());
            for j in 0..16 {
                block[j] ^= prev[j];
            }
            dec[i * 16..(i + 1) * 16].copy_from_slice(&block);
            prev = chunk.to_vec();
        }
        assert_eq!(dec, payload);
    }

    #[test]
    fn aes_cbc_trailing_bytes_sent_clear() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let payload = [3u8; 20]; // one full block + 4 trailing bytes
        let packet = build_buffered_audio_packet_aes(1, 2, 0x16000000, &key, &iv, &payload).unwrap();
        let enc = &packet[12..];
        assert_eq!(enc.len(), 20);
        // First 16 bytes should be encrypted (not equal to plaintext), last 4 in clear.
        assert_ne!(&enc[..16], &payload[..16]);
        assert_eq!(&enc[16..], &payload[16..]);
    }
}
