//! Float32 → interleaved PCM frame encoding. 16-bit output is big-endian
//! (L16 network order, as pyatv byteswaps before sending); 24-bit honours the
//! requested endianness.

pub const FRAME_SIZE: usize = 352;

pub fn quantize(sample: f32, bits: u32) -> i32 {
    let scale = (1u64 << (bits - 1)) as f64;
    (sample as f64 * scale).round().clamp(-scale, scale - 1.0) as i32
}

pub struct PcmEncoder {
    pub sample_rate: i32,
    pub channels: i32,
    pub bits_per_sample: i32,
    pub little_endian: bool,
}

impl PcmEncoder {
    pub fn new(sample_rate: i32, channels: i32, bits_per_sample: i32, little_endian: bool) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_sample,
            little_endian,
        }
    }

    pub fn frame_size(&self) -> usize {
        FRAME_SIZE
    }

    /// Encode the provided interleaved samples into one PCM frame.
    pub fn encode(&self, samples: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * (self.bits_per_sample as usize / 8));
        for &s in samples {
            if self.bits_per_sample == 16 {
                let val = quantize(s, 16) as i16;
                out.extend_from_slice(&val.to_be_bytes());
                continue;
            }
            // 24-bit
            let b = quantize(s, 24) as u32;
            if self.little_endian {
                out.push(b as u8);
                out.push((b >> 8) as u8);
                out.push((b >> 16) as u8);
            } else {
                out.push((b >> 16) as u8);
                out.push((b >> 8) as u8);
                out.push(b as u8);
            }
        }
        out
    }
}
