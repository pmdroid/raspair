//! ffmpeg-backed decoding for non-WAV inputs (FLAC, MP3, M4A, ...). Decodes to
//! interleaved float32 resampled to the requested rate and channel count.

use anyhow::{anyhow, bail, Result};
use std::process::Command;

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

/// Returns (samples, sample_rate, channels).
pub fn decode_audio_file(path: &str, target_sample_rate: i32, target_channels: i32) -> Result<(Vec<f32>, i32, i32)> {
    if !ffmpeg_available() {
        bail!("ffmpeg not found in PATH");
    }

    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path,
            "-vn",
            "-ac",
            &target_channels.to_string(),
            "-ar",
            &target_sample_rate.to_string(),
            "-c:a",
            "pcm_s24le",
            "-f",
            "s24le",
            "pipe:1",
        ])
        .output()
        .map_err(|e| anyhow!("ffmpeg start: {e}"))?;

    if !out.status.success() {
        bail!("ffmpeg decode {path}: {}", String::from_utf8_lossy(&out.stderr));
    }
    if out.stdout.is_empty() {
        bail!("ffmpeg produced no output for {path}");
    }

    let samples = out
        .stdout
        .chunks_exact(3)
        .map(|b| {
            let mut val = (b[0] as i32) | (b[1] as i32) << 8 | (b[2] as i32) << 16;
            if val & 0x800000 != 0 {
                val |= !0xFFFFFF;
            }
            val as f32 / 8_388_608.0
        })
        .collect();

    Ok((samples, target_sample_rate, target_channels))
}
