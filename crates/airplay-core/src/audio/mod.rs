//! Audio codecs and file decoding: PCM framing, WAV parsing, ffmpeg decode,
//! AAC-LC (ADTS) and ALAC escape frames.

pub mod aac;
pub mod alac;
pub mod decode;
pub mod pcm;
pub mod wav;

pub use aac::{encode_aac_file, parse_adts_frames, AAC_LC_44100_STEREO_ASC, AAC_LC_48000_STEREO_ASC};
pub use alac::{alac_magic_cookie, encode_alac_escape_frame};
pub use decode::{decode_audio_file, ffmpeg_available, AudioInfo, AudioReader, DecodedBlock, Samples, SeekResult};
pub use symphonia::core::io::MediaSource;
pub use pcm::PcmEncoder;

use anyhow::Result;

/// Decode any audio file to interleaved float32. WAV uses the native decoder
/// for fidelity; everything else goes through ffmpeg at the target format.
pub fn load_audio(path: &str, target_sr: i32, target_ch: i32) -> Result<(Vec<f32>, i32, i32)> {
    if let Ok(data) = std::fs::read(path) {
        if let Ok(decoded) = wav::decode(&data) {
            return Ok(decoded);
        }
    }
    let mut reader = AudioReader::open(path)?;
    let info = reader.info().clone();
    if info.sample_rate as i32 != target_sr || info.channels as i32 != target_ch {
        return decode_audio_file(path, target_sr, target_ch);
    }
    let mut samples = Vec::new();
    while let Some(block) = reader.next_block()? {
        samples.extend(block.samples.to_f32());
    }
    Ok((samples, target_sr, target_ch))
}
