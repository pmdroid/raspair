//! ffmpeg-backed decoding for non-WAV inputs (FLAC, MP3, M4A, ...). Decodes to
//! interleaved float32 resampled to the requested rate and channel count.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::path::Path;
use std::process::Command;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::well_known::CODEC_ID_ALAC;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::{TimeBase, Timestamp};

const MAX_TAGS: usize = 64;
const MAX_TAG_CHARS: usize = 1024;
const MAX_DECODE_ERRORS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub enum Samples {
    Int { bits: u32, data: Vec<i32> },
    Float(Vec<f32>),
}

impl Samples {
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Samples::Int { bits, data } => {
                let scale = (1u64 << (bits - 1)) as f32;
                data.iter().map(|&s| s as f32 / scale).collect()
            }
            Samples::Float(data) => data.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedBlock {
    pub start_frame: u64,
    pub frames: usize,
    pub samples: Samples,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: Option<u32>,
    pub total_frames: Option<u64>,
    pub delay_frames: u32,
    pub padding_frames: u32,
    pub seekable: bool,
    pub tags: Vec<(String, String)>,
}

impl AudioInfo {
    pub fn duration_secs(&self) -> Option<f64> {
        self.total_frames.map(|f| f as f64 / self.sample_rate as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekResult {
    pub requested_frame: u64,
    pub seeked_frame: u64,
    pub start_frame: u64,
}

pub struct AudioReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    info: AudioInfo,
    next_frame: u64,
    skip_until: u64,
}

impl AudioReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let ext = path.extension().and_then(|e| e.to_str());
        Self::from_source(Box::new(file), ext).with_context(|| format!("decode {}", path.display()))
    }

    pub fn from_source(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<Self> {
        let seekable = source.is_seekable();
        let mut hint = Hint::new();
        if let Some(ext) = extension {
            hint.with_extension(ext);
        }
        let mss = MediaSourceStream::new(source, Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| anyhow!("unsupported or malformed audio: {e}"))?;
        let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("missing audio codec parameters"))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| anyhow!("unsupported codec {}: {e}", params.codec))?;
        let sample_rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
        let channels = params.channels.as_ref().map(|c| c.count()).ok_or_else(|| anyhow!("unknown channel layout"))?;
        if sample_rate == 0 || channels == 0 || channels > u16::MAX as usize {
            bail!("invalid audio format {sample_rate} Hz / {channels} ch");
        }
        let bits_per_sample = params.bits_per_sample.or_else(|| match params.codec {
            CODEC_ID_ALAC => params.extra_data.as_deref().and_then(alac_bit_depth),
            _ => None,
        });
        let codec = symphonia::default::get_codecs()
            .get_audio_decoder(params.codec)
            .map(|d| d.codec.info.short_name.to_string())
            .unwrap_or_else(|| params.codec.to_string());
        let mut info = AudioInfo {
            codec,
            sample_rate,
            channels: channels as u16,
            bits_per_sample,
            total_frames: track.num_frames,
            delay_frames: track.delay.unwrap_or(0),
            padding_frames: track.padding.unwrap_or(0),
            seekable,
            tags: Vec::new(),
        };
        let track_id = track.id;
        let time_base = track.time_base;
        info.tags = collect_tags(format.as_mut());
        Ok(Self { format, decoder, track_id, time_base, info, next_frame: 0, skip_until: 0 })
    }

    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    pub fn next_block(&mut self) -> Result<Option<DecodedBlock>> {
        let mut errors = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => bail!("read packet: {e}"),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let (frames, samples) = match self.decoder.decode(&packet) {
                Ok(buf) => (buf.frames(), convert(&buf, self.info.bits_per_sample)),
                Err(SymphoniaError::DecodeError(msg)) => {
                    errors += 1;
                    if errors > MAX_DECODE_ERRORS {
                        bail!("malformed audio: {msg}");
                    }
                    continue;
                }
                Err(e) => bail!("decode: {e}"),
            };
            let start = self.next_frame;
            let frames = match self.info.total_frames {
                Some(total) if start >= total => return Ok(None),
                Some(total) => frames.min((total - start) as usize),
                None => frames,
            };
            self.next_frame += frames as u64;
            let skip = self.skip_until.saturating_sub(start).min(frames as u64) as usize;
            if skip == frames {
                continue;
            }
            let ch = self.info.channels as usize;
            let range = skip * ch..frames * ch;
            let samples = match samples {
                Samples::Int { bits, data } => Samples::Int { bits, data: data[range].to_vec() },
                Samples::Float(data) => Samples::Float(data[range].to_vec()),
            };
            return Ok(Some(DecodedBlock { start_frame: start + skip as u64, frames: frames - skip, samples }));
        }
    }

    pub fn seek(&mut self, frame: u64) -> Result<SeekResult> {
        if !self.info.seekable {
            bail!("source is not seekable");
        }
        let ts = self.frames_to_ts(frame);
        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: self.track_id })
            .map_err(|e| anyhow!("seek to frame {frame}: {e}"))?;
        self.decoder.reset();
        let seeked_frame = self.ts_to_frames(seeked.actual_ts);
        self.next_frame = seeked_frame;
        self.skip_until = frame;
        Ok(SeekResult { requested_frame: frame, seeked_frame, start_frame: seeked_frame.max(frame) })
    }

    fn frames_to_ts(&self, frames: u64) -> i64 {
        match self.time_base {
            Some(tb) => {
                let ts = frames as u128 * tb.denom.get() as u128 / (tb.numer.get() as u128 * self.info.sample_rate as u128);
                ts.min(i64::MAX as u128) as i64
            }
            None => frames.min(i64::MAX as u64) as i64,
        }
    }

    fn ts_to_frames(&self, ts: Timestamp) -> u64 {
        let ts = ts.get().max(0) as u128;
        match self.time_base {
            Some(tb) => {
                let frames = ts * tb.numer.get() as u128 * self.info.sample_rate as u128 / tb.denom.get() as u128;
                frames.min(u64::MAX as u128) as u64
            }
            None => ts as u64,
        }
    }
}

fn convert(buf: &GenericAudioBufferRef<'_>, bits: Option<u32>) -> Samples {
    let native = match buf {
        GenericAudioBufferRef::F32(_) | GenericAudioBufferRef::F64(_) => {
            let mut data = Vec::new();
            buf.copy_to_vec_interleaved::<f32>(&mut data);
            return Samples::Float(data);
        }
        GenericAudioBufferRef::U8(_) | GenericAudioBufferRef::S8(_) => 8,
        GenericAudioBufferRef::U16(_) | GenericAudioBufferRef::S16(_) => 16,
        GenericAudioBufferRef::U24(_) | GenericAudioBufferRef::S24(_) => 24,
        _ => 32,
    };
    let bits = bits.unwrap_or(native).clamp(1, native);
    let mut data = Vec::new();
    buf.copy_to_vec_interleaved::<i32>(&mut data);
    let shift = 32 - bits;
    if shift > 0 {
        data.iter_mut().for_each(|s| *s >>= shift);
    }
    Samples::Int { bits, data }
}

fn alac_bit_depth(mut cookie: &[u8]) -> Option<u32> {
    for atom in [b"frma", b"alac"] {
        if cookie.len() >= 12 && &cookie[4..8] == atom {
            cookie = &cookie[12..];
        }
    }
    (cookie.len() >= 24).then(|| cookie[5] as u32).filter(|b| (1..=32).contains(b))
}

fn collect_tags(format: &mut dyn FormatReader) -> Vec<(String, String)> {
    let mut metadata = format.metadata();
    let Some(rev) = metadata.skip_to_latest() else {
        return Vec::new();
    };
    rev.media
        .tags
        .iter()
        .chain(rev.per_track.iter().flat_map(|t| t.metadata.tags.iter()))
        .take(MAX_TAGS)
        .map(|tag| {
            let key = match &tag.std {
                Some(StandardTag::TrackTitle(_)) => "title".to_string(),
                Some(StandardTag::Artist(_)) => "artist".to_string(),
                Some(StandardTag::Album(_)) => "album".to_string(),
                _ => tag.raw.key.chars().take(MAX_TAG_CHARS).collect(),
            };
            (key, tag.raw.value.to_string().chars().take(MAX_TAG_CHARS).collect())
        })
        .collect()
}

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
