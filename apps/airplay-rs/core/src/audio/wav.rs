//! Minimal WAV decoder (PCM 16/24/32-bit, mono/stereo) returning interleaved
//! float32 samples.

use anyhow::{anyhow, bail, Result};

const PCM_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Returns (samples, sample_rate, channels).
pub fn decode(data: &[u8]) -> Result<(Vec<f32>, i32, i32)> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }

    let mut audio_format = 0u16;
    let mut num_channels = 0i32;
    let mut sample_rate = 0i32;
    let mut bits_per_sample = 0i32;
    let mut is_extensible_pcm = false;
    let mut have_fmt = false;
    let mut raw: Option<&[u8]> = None;

    let mut off = 12usize;
    while off + 8 <= data.len() {
        let chunk_id = &data[off..off + 4];
        let chunk_size = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 8;
        if off + chunk_size > data.len() {
            break;
        }
        let chunk = &data[off..off + chunk_size];

        match chunk_id {
            b"fmt " => {
                if chunk.len() < 16 {
                    bail!("invalid fmt chunk size: {}", chunk.len());
                }
                audio_format = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
                num_channels = u16::from_le_bytes(chunk[2..4].try_into().unwrap()) as i32;
                sample_rate = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as i32;
                bits_per_sample = u16::from_le_bytes(chunk[14..16].try_into().unwrap()) as i32;
                if audio_format == 0xFFFE {
                    is_extensible_pcm = chunk.len() >= 40 && chunk[24..40] == PCM_GUID;
                }
                have_fmt = true;
            }
            b"data" => raw = Some(chunk),
            _ => {}
        }

        off += chunk_size + (chunk_size & 1); // word-align
        if have_fmt && raw.is_some() {
            break;
        }
    }

    if !have_fmt {
        bail!("missing WAV fmt chunk");
    }
    let raw = raw.ok_or_else(|| anyhow!("missing WAV data chunk"))?;

    if audio_format == 0xFFFE {
        if !is_extensible_pcm {
            bail!("unsupported WAVE_FORMAT_EXTENSIBLE subformat");
        }
    } else if audio_format != 1 {
        bail!("unsupported audio format: {audio_format}");
    }
    if !(1..=2).contains(&num_channels) {
        bail!("unsupported channels: {num_channels}");
    }

    let samples = match bits_per_sample {
        16 => raw
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect(),
        24 => raw
            .chunks_exact(3)
            .map(|b| {
                let mut val = (b[0] as i32) | (b[1] as i32) << 8 | (b[2] as i32) << 16;
                if val & 0x800000 != 0 {
                    val |= !0xFFFFFF;
                }
                val as f32 / 8_388_608.0
            })
            .collect(),
        32 => raw
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        other => bail!("unsupported bits per sample: {other}"),
    };

    Ok((samples, sample_rate, num_channels))
}
