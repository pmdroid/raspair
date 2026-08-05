//! AAC-LC support for the buffered path: ffmpeg encoding (via Apple's
//! `aac_at`), ADTS framing, and the AudioSpecificConfig blobs sent in SETUP.

use anyhow::{anyhow, bail, Result};
use std::process::Command;

pub const AAC_LC_44100_STEREO_ASC: [u8; 2] = [0x12, 0x10];
pub const AAC_LC_48000_STEREO_ASC: [u8; 2] = [0x11, 0x90];

/// Encode any input file to raw AAC frames (ADTS headers stripped), ready for
/// the buffered transport.
pub fn encode_aac_file(path: &str, sample_rate: i32, channels: i32) -> Result<Vec<Vec<u8>>> {
    encode_aac(path, sample_rate, channels, false)
}

fn encode_aac(path: &str, sample_rate: i32, channels: i32, keep_adts: bool) -> Result<Vec<Vec<u8>>> {
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path,
            "-vn",
            "-ac",
            &channels.to_string(),
            "-ar",
            &sample_rate.to_string(),
            "-c:a",
            "aac_at",
            "-b:a",
            "128k",
            "-f",
            "adts",
            "pipe:1",
        ])
        .output()
        .map_err(|e| anyhow!("ffmpeg encode AAC start: {e}"))?;

    if !out.status.success() {
        bail!("ffmpeg encode AAC: {}", String::from_utf8_lossy(&out.stderr));
    }

    let mut frames = parse_adts(&out.stdout, keep_adts)?;
    if frames.is_empty() {
        bail!("ffmpeg produced no AAC frames");
    }
    // Skip leading init frames (tiny priming frames).
    let mut first_audio = 0;
    while first_audio < frames.len() && first_audio < 2 && frames[first_audio].len() < 35 {
        first_audio += 1;
    }
    Ok(frames.split_off(first_audio))
}

pub fn parse_adts_frames(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    parse_adts(data, false)
}

fn parse_adts(data: &[u8], keep_headers: bool) -> Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    let mut off = 0usize;
    while off < data.len() {
        if off + 7 > data.len() {
            bail!("truncated ADTS header at offset {off}");
        }
        if data[off] != 0xFF || data[off + 1] & 0xF0 != 0xF0 {
            bail!("invalid ADTS sync word at offset {off}");
        }
        let protection_absent = data[off + 1] & 0x01 == 1;
        let header_len = if protection_absent { 7 } else { 9 };
        let frame_len = ((data[off + 3] as usize & 0x03) << 11)
            | ((data[off + 4] as usize) << 3)
            | ((data[off + 5] as usize & 0xE0) >> 5);
        if frame_len < header_len {
            bail!("invalid ADTS frame length {frame_len} at offset {off}");
        }
        if off + frame_len > data.len() {
            bail!("truncated ADTS frame at offset {off}");
        }
        if keep_headers {
            frames.push(data[off..off + frame_len].to_vec());
        } else {
            frames.push(data[off + header_len..off + frame_len].to_vec());
        }
        off += frame_len;
    }
    Ok(frames)
}
