use super::decode::Samples;
use anyhow::{anyhow, bail, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType, WindowFunction};

pub const SOURCE_RATES: [u32; 4] = [44_100, 48_000, 96_000, 192_000];
pub const OUTPUT_RATES: [u32; 2] = [44_100, 48_000];
pub const OUTPUT_CHANNELS: usize = 2;
const CHUNK_FRAMES: usize = 1024;
const FLOAT_VALID_BITS: u32 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub storage_bits: u32,
    pub valid_bits: u32,
    pub float: bool,
}

impl SourceFormat {
    pub fn int(sample_rate: u32, channels: u16, storage_bits: u32, valid_bits: u32) -> Self {
        Self { sample_rate, channels, storage_bits, valid_bits, float: false }
    }

    pub fn float(sample_rate: u32, channels: u16) -> Self {
        Self { sample_rate, channels, storage_bits: 32, valid_bits: FLOAT_VALID_BITS, float: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputFormat {
    pub sample_rate: u32,
    pub bits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConversionPlan {
    pub resample: bool,
    pub dither: bool,
    pub exact: bool,
    pub delay_frames: usize,
}

struct Dither(u64);

impl Dither {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn tpdf(&mut self) -> f64 {
        self.uniform() - self.uniform()
    }
}

pub struct Converter {
    source: SourceFormat,
    output: OutputFormat,
    gain: f32,
    scale: f64,
    resampler: Option<Async<f32>>,
    dither: Option<Dither>,
    pending: Vec<f32>,
    staged: Vec<f32>,
    out: Vec<f32>,
    skip: usize,
    source_frames: u64,
    output_frames: u64,
    finished: bool,
}

impl Converter {
    pub fn new(source: SourceFormat, output: OutputFormat, gain: f32, dither_seed: u64) -> Result<Self> {
        validate(&source, &output)?;
        if !gain.is_finite() || gain < 0.0 {
            bail!("invalid gain {gain}");
        }
        let resampler = if source.sample_rate == output.sample_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: None,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Cubic,
                window: WindowFunction::BlackmanHarris2,
            };
            let ratio = output.sample_rate as f64 / source.sample_rate as f64;
            Some(
                Async::<f32>::new_sinc(ratio, 1.0, &params, CHUNK_FRAMES, OUTPUT_CHANNELS, FixedAsync::Input)
                    .map_err(|e| anyhow!("resampler: {e}"))?,
            )
        };
        let staged = vec![0.0; resampler.as_ref().map_or(0, |r| r.output_frames_max()) * OUTPUT_CHANNELS];
        let skip = resampler.as_ref().map_or(0, |r| r.output_delay());
        let dither = (source.valid_bits > output.bits)
            .then(|| Dither(dither_seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1));
        Ok(Self {
            source,
            output,
            gain,
            scale: (1u64 << (output.bits - 1)) as f64,
            resampler,
            dither,
            pending: Vec::with_capacity(CHUNK_FRAMES * OUTPUT_CHANNELS),
            staged,
            out: Vec::with_capacity(CHUNK_FRAMES * OUTPUT_CHANNELS * 5),
            skip,
            source_frames: 0,
            output_frames: 0,
            finished: false,
        })
    }

    pub fn plan(&self) -> ConversionPlan {
        ConversionPlan {
            resample: self.resampler.is_some(),
            dither: self.dither.is_some(),
            exact: self.resampler.is_none() && self.dither.is_none() && self.gain == 1.0,
            delay_frames: self.delay_frames(),
        }
    }

    pub fn source(&self) -> SourceFormat {
        self.source
    }

    pub fn output(&self) -> OutputFormat {
        self.output
    }

    pub fn delay_frames(&self) -> usize {
        self.resampler.as_ref().map_or(0, |r| r.output_delay())
    }

    pub fn output_frames_for(&self, source_frames: u64) -> u64 {
        let (src, out) = (self.source.sample_rate as u128, self.output.sample_rate as u128);
        ((source_frames as u128 * out).div_ceil(src)) as u64
    }

    pub fn source_frame_at(&self, output_frame: u64) -> u64 {
        let (src, out) = (self.source.sample_rate as u128, self.output.sample_rate as u128);
        (output_frame as u128 * src / out) as u64
    }

    pub fn source_frames(&self) -> u64 {
        self.source_frames
    }

    pub fn output_frames(&self) -> u64 {
        self.output_frames
    }

    pub fn push(&mut self, samples: &Samples) -> Result<&[f32]> {
        match samples {
            Samples::Int { bits, data } if !self.source.float && *bits == self.source.valid_bits => {
                self.check_len(data.len())?;
                let inv = 1.0 / (1u64 << (bits - 1)) as f32;
                self.feed(data.iter().map(|&v| v as f32 * inv))
            }
            Samples::Float(data) if self.source.float => {
                self.check_len(data.len())?;
                self.feed(data.iter().copied())
            }
            _ => bail!("samples do not match source format {:?}", self.source),
        }
    }

    pub fn push_le_bytes(&mut self, bytes: &[u8]) -> Result<&[f32]> {
        let width = self.source.storage_bits as usize / 8;
        if !bytes.len().is_multiple_of(width) {
            bail!("{} bytes is not a whole number of {width}-byte samples", bytes.len());
        }
        self.check_len(bytes.len() / width)?;
        if self.source.float {
            return self.feed(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
        }
        let shift = 32 - self.source.valid_bits;
        let inv = 1.0 / (1u64 << (self.source.valid_bits - 1)) as f32;
        self.feed(bytes.chunks_exact(width).map(|c| {
            let mut w = [0u8; 4];
            w[4 - width..].copy_from_slice(c);
            (i32::from_le_bytes(w) >> shift) as f32 * inv
        }))
    }

    pub fn finish(&mut self) -> Result<&[f32]> {
        self.out.clear();
        if self.finished || self.resampler.is_none() {
            self.finished = true;
            return Ok(&self.out);
        }
        self.finished = true;
        let target = self.output_frames_for(self.source_frames);
        let mut partial = self.pending.len() / OUTPUT_CHANNELS;
        while self.output_frames < target {
            self.run(Some(partial))?;
            partial = 0;
        }
        Ok(&self.out)
    }

    fn check_len(&self, samples: usize) -> Result<()> {
        if self.finished {
            bail!("converter already finished");
        }
        if !samples.is_multiple_of(self.source.channels as usize) {
            bail!("{samples} samples is not a whole number of {}-channel frames", self.source.channels);
        }
        Ok(())
    }

    fn feed(&mut self, mut values: impl Iterator<Item = f32>) -> Result<&[f32]> {
        self.out.clear();
        let mono = self.source.channels == 1;
        while let Some(l) = values.next() {
            let r = if mono { l } else { values.next().unwrap_or(0.0) };
            let (l, r) = if self.gain == 1.0 { (l, r) } else { (l * self.gain, r * self.gain) };
            self.source_frames += 1;
            if self.resampler.is_none() {
                quantize(&mut self.out, &mut self.dither, self.scale, &[l, r]);
                self.output_frames += 1;
                continue;
            }
            self.pending.extend_from_slice(&[l, r]);
            if self.pending.len() == CHUNK_FRAMES * OUTPUT_CHANNELS {
                self.run(None)?;
            }
        }
        Ok(&self.out)
    }

    fn run(&mut self, partial: Option<usize>) -> Result<()> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(());
        };
        self.pending.resize(CHUNK_FRAMES * OUTPUT_CHANNELS, 0.0);
        let frames_out = self.staged.len() / OUTPUT_CHANNELS;
        let input = InterleavedSlice::new(&self.pending[..], OUTPUT_CHANNELS, CHUNK_FRAMES).map_err(|e| anyhow!("{e}"))?;
        let mut output =
            InterleavedSlice::new_mut(&mut self.staged[..], OUTPUT_CHANNELS, frames_out).map_err(|e| anyhow!("{e}"))?;
        let indexing = partial.map(|n| Indexing::new().partial_len(n));
        let (_, written) = resampler.process_into_buffer(&input, &mut output, indexing.as_ref())?;
        self.pending.clear();
        if written == 0 {
            bail!("resampler produced no output");
        }
        let skip = self.skip.min(written);
        self.skip -= skip;
        let room = self.output_frames_for(self.source_frames).saturating_sub(self.output_frames) as usize;
        let keep = (written - skip).min(room);
        let staged = &self.staged[skip * OUTPUT_CHANNELS..(skip + keep) * OUTPUT_CHANNELS];
        quantize(&mut self.out, &mut self.dither, self.scale, staged);
        self.output_frames += keep as u64;
        Ok(())
    }
}

fn quantize(out: &mut Vec<f32>, dither: &mut Option<Dither>, scale: f64, values: &[f32]) {
    for &x in values {
        let d = dither.as_mut().map_or(0.0, Dither::tpdf);
        let q = (x as f64 * scale + d).round().clamp(-scale, scale - 1.0);
        out.push((q / scale) as f32);
    }
}

fn validate(source: &SourceFormat, output: &OutputFormat) -> Result<()> {
    if !SOURCE_RATES.contains(&source.sample_rate) {
        bail!("unsupported source rate {} Hz", source.sample_rate);
    }
    if !OUTPUT_RATES.contains(&output.sample_rate) {
        bail!("unsupported output rate {} Hz", output.sample_rate);
    }
    if !matches!(source.channels, 1 | 2) {
        bail!("unsupported channel layout: {} channels", source.channels);
    }
    if !matches!(output.bits, 16 | 24) {
        bail!("unsupported output depth {} bits", output.bits);
    }
    let storage_ok = if source.float { source.storage_bits == 32 } else { matches!(source.storage_bits, 16 | 24 | 32) };
    if !storage_ok || source.valid_bits == 0 || source.valid_bits > source.storage_bits {
        bail!("unsupported sample format: {} valid bits in {} bit storage", source.valid_bits, source.storage_bits);
    }
    Ok(())
}
