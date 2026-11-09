//! RTP header construction and small random helpers.

use rand::RngCore;

pub fn build_rtp_header(seq: u16, ts: u32, ssrc: u32, marker: bool, pt: i32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80; // version 2
    h[1] = (pt & 0x7F) as u8;
    if marker {
        h[1] |= 0x80;
    }
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&ts.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

pub fn random_u16() -> u16 {
    (rand::thread_rng().next_u32() & 0xFFFF) as u16
}
