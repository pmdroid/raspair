//! ALAC "escape" (uncompressed) framing plus the magic cookie. Escape frames
//! are valid ALAC bitstream — every decoder accepts them — so this exercises
//! the buffered ALAC envelope (cookie/ssrc/ct/framing) end to end without a
//! full encoder. Layout verified against macosforge/alac ALACEncoder.cpp.

const ALAC_ID_CPE: u32 = 1;
const ALAC_ID_END: u32 = 7;

/// 24-byte big-endian ALACSpecificConfig for a stereo stream. pb/mb/kb/maxRun
/// are the codec defaults (40/10/14/255).
pub fn alac_magic_cookie(frame_length: u32, bit_depth: u8, channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    b[0..4].copy_from_slice(&frame_length.to_be_bytes());
    b[4] = 0; // compatibleVersion
    b[5] = bit_depth;
    b[6] = 40; // pb
    b[7] = 10; // mb
    b[8] = 14; // kb
    b[9] = channels;
    b[10..12].copy_from_slice(&255u16.to_be_bytes()); // maxRun
    // maxFrameBytes / avgBitRate stay zero (unknown).
    b[20..24].copy_from_slice(&sample_rate.to_be_bytes());
    b
}

struct BitWriter {
    buf: Vec<u8>,
    nbit: usize,
}

impl BitWriter {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            nbit: 0,
        }
    }

    fn write(&mut self, v: u32, mut bits: usize) {
        while bits > 0 {
            if self.nbit % 8 == 0 {
                self.buf.push(0);
            }
            let free = 8 - self.nbit % 8;
            let n = bits.min(free);
            let chunk = (v >> (bits - n)) & ((1 << n) - 1);
            let last = self.buf.len() - 1;
            self.buf[last] |= (chunk << (free - n)) as u8;
            self.nbit += n;
            bits -= n;
        }
    }

    fn byte_align(&mut self) {
        if self.nbit % 8 != 0 {
            self.nbit += 8 - self.nbit % 8;
        }
    }
}

/// Wrap interleaved stereo float32 samples in a 24-bit ALAC escape frame. The
/// float-to-int conversion matches the PCM encoder, so escape output is
/// bit-identical to the 24-bit PCM path.
pub fn encode_alac_escape_frame(samples: &[f32], spf: usize) -> Vec<u8> {
    let mut num_samples = samples.len() / 2;
    if num_samples > spf {
        num_samples = spf;
    }
    let mut w = BitWriter::new(num_samples * 6 + 8);

    w.write(ALAC_ID_CPE, 3);
    w.write(0, 4); // stereo element instance tag
    w.write(0, 12); // unused header, must be zero
    let partial: u32 = if num_samples != spf { 1 } else { 0 };
    w.write(partial << 3 | 1, 4); // bytesShifted=0, escape=1
    if partial == 1 {
        w.write(num_samples as u32, 32);
    }

    for &s in &samples[..num_samples * 2] {
        let s = s.clamp(-1.0, 1.0);
        let mut val = (s * 8_388_607.0) as i32;
        if s <= -1.0 {
            val = -8_388_608;
        }
        w.write((val as u32) & 0xFFFFFF, 24);
    }

    w.write(ALAC_ID_END, 3);
    w.byte_align();
    w.buf
}
