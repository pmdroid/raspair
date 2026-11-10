use crate::audio::AudioInfo;
use crate::client::{AudioCodec, BufferedAudioFormat, StreamType};
use crate::protocol::{Route, StreamCandidate};
use crate::session::{SessionError, SessionProfile};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputProfile {
    pub transport: StreamType,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub valid_bits: u16,
    pub channels: u16,
}

impl OutputProfile {
    pub fn of(profile: &SessionProfile) -> Result<Self, SessionError> {
        match profile {
            SessionProfile::Realtime { sample_rate } if [44100, 48000].contains(sample_rate) => Ok(Self {
                transport: StreamType::Realtime,
                codec: AudioCodec::Pcm,
                sample_rate: *sample_rate as u32,
                valid_bits: 16,
                channels: 2,
            }),
            SessionProfile::Buffered(f) if known_buffered().iter().any(|k| same_format(k, f)) => Ok(Self {
                transport: StreamType::Buffered,
                codec: f.codec,
                sample_rate: f.sample_rate as u32,
                valid_bits: f.bits_per_sample as u16,
                channels: f.channels as u16,
            }),
            _ => Err(SessionError::UnsupportedProfile),
        }
    }

    pub fn lossless(&self) -> bool {
        self.codec != AudioCodec::Aac && self.valid_bits > 0
    }

    pub fn pcm_bitrate(&self) -> u64 {
        self.sample_rate as u64 * self.valid_bits as u64 * self.channels as u64
    }

    pub fn is_buffered_lossless_24_48(&self) -> bool {
        self.transport == StreamType::Buffered && self.lossless() && self.sample_rate == 48000 && self.valid_bits == 24 && self.channels == 2
    }
}

impl fmt::Display for OutputProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = match self.transport {
            StreamType::Buffered => "buffered",
            StreamType::Realtime => "realtime",
        };
        let codec = match self.codec {
            AudioCodec::Pcm => "PCM",
            AudioCodec::Alac => "ALAC",
            AudioCodec::Aac => "AAC",
        };
        write!(f, "{transport} {codec} {} Hz", self.sample_rate)?;
        if self.valid_bits > 0 {
            write!(f, "/{}-bit", self.valid_bits)?;
        }
        write!(f, "/{}ch", self.channels)?;
        if self.lossless() {
            write!(f, " ({} bit/s PCM, lossless)", self.pcm_bitrate())
        } else {
            write!(f, " (lossy)")
        }
    }
}

fn known_buffered() -> [BufferedAudioFormat; 5] {
    [
        BufferedAudioFormat::pcm_48000_24_stereo(true),
        BufferedAudioFormat::pcm_48000_24_stereo(false),
        BufferedAudioFormat::alac_48000_24_stereo(),
        BufferedAudioFormat::aac_44100_stereo(),
        BufferedAudioFormat::aac_48000_stereo(),
    ]
}

fn same_format(a: &BufferedAudioFormat, b: &BufferedAudioFormat) -> bool {
    a.codec == b.codec
        && a.sample_rate == b.sample_rate
        && a.channels == b.channels
        && a.bits_per_sample == b.bits_per_sample
        && a.little_endian == b.little_endian
        && a.samples_per_frame == b.samples_per_frame
        && a.audio_format == b.audio_format
        && a.asc == b.asc
}

#[derive(Debug, Clone, Default)]
pub enum FallbackPolicy {
    #[default]
    Forbid,
    Allow(Vec<SessionProfile>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    CapabilitiesUnknown,
    NotAdvertised,
    SetupRejected { status: u16 },
    RateNotValidated { sample_rate: u32 },
}

impl FallbackReason {
    fn into_error(self) -> SessionError {
        match self {
            FallbackReason::SetupRejected { status } => SessionError::ReceiverRejected { status },
            FallbackReason::RateNotValidated { sample_rate } => SessionError::RealtimeRate { requested: sample_rate },
            _ => SessionError::UnsupportedProfile,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSelection {
    pub requested: OutputProfile,
    pub effective: OutputProfile,
    pub fallback: Option<FallbackReason>,
}

pub trait ProfileTarget {
    fn route(&self) -> &Route;
    fn setup(&mut self, profile: &SessionProfile) -> Result<(), SessionError>;
}

pub fn advertised(route: &Route, profile: &SessionProfile) -> Result<(), FallbackReason> {
    let wanted = match profile {
        SessionProfile::Realtime { .. } => StreamCandidate::Realtime,
        SessionProfile::Buffered(_) => StreamCandidate::Buffered,
    };
    if route.streams.is_empty() {
        Err(FallbackReason::CapabilitiesUnknown)
    } else if !route.streams.contains(&wanted) {
        Err(FallbackReason::NotAdvertised)
    } else if let SessionProfile::Realtime { sample_rate } = profile {
        let sample_rate = *sample_rate as u32;
        match route.realtime_rates.contains(&sample_rate) {
            true => Ok(()),
            false => Err(FallbackReason::RateNotValidated { sample_rate }),
        }
    } else {
        Ok(())
    }
}

pub fn candidates<'a>(requested: &'a SessionProfile, policy: &'a FallbackPolicy) -> Result<Vec<&'a SessionProfile>, SessionError> {
    let mut all = vec![requested];
    if let FallbackPolicy::Allow(more) = policy {
        all.extend(more);
    }
    for profile in &all {
        OutputProfile::of(profile)?;
    }
    Ok(all)
}

pub fn preflight(route: &Route, requested: &SessionProfile, policy: &FallbackPolicy) -> Result<(), SessionError> {
    let all = candidates(requested, policy)?;
    match all.iter().map(|p| advertised(route, p)).find(|r| r.is_ok()) {
        Some(_) => Ok(()),
        None => Err(advertised(route, requested).unwrap_err().into_error()),
    }
}

pub fn negotiate(
    target: &mut dyn ProfileTarget,
    requested: &SessionProfile,
    policy: &FallbackPolicy,
) -> Result<(SessionProfile, ProfileSelection), SessionError> {
    let requested_output = OutputProfile::of(requested)?;
    let mut first_reason = None;
    for profile in candidates(requested, policy)? {
        let reason = match advertised(target.route(), profile) {
            Err(reason) => reason,
            Ok(()) => match target.setup(profile) {
                Ok(()) => {
                    let effective = OutputProfile::of(profile)?;
                    if first_reason.is_none() && effective != requested_output {
                        return Err(SessionError::UnsupportedProfile);
                    }
                    let selection = ProfileSelection { requested: requested_output, effective, fallback: first_reason };
                    return Ok((profile.clone(), selection));
                }
                Err(SessionError::ReceiverRejected { status }) => FallbackReason::SetupRejected { status },
                Err(other) => return Err(other),
            },
        };
        first_reason.get_or_insert(reason);
    }
    Err(first_reason.map(FallbackReason::into_error).unwrap_or(SessionError::UnsupportedProfile))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: Option<u32>,
    pub lossless: bool,
}

impl From<&AudioInfo> for SourceFormat {
    fn from(info: &AudioInfo) -> Self {
        let codec = info.codec.to_ascii_lowercase();
        let lossless = codec.starts_with("pcm") || codec == "flac" || codec == "alac";
        Self {
            codec: info.codec.clone(),
            sample_rate: info.sample_rate,
            channels: info.channels,
            bits_per_sample: info.bits_per_sample,
            lossless,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityReport {
    pub source: SourceFormat,
    pub output: OutputProfile,
    pub resampled: bool,
    pub requantized: bool,
    pub remixed: bool,
    pub lossless_end_to_end: bool,
}

impl QualityReport {
    pub fn new(source: SourceFormat, output: OutputProfile) -> Self {
        let resampled = source.sample_rate != output.sample_rate;
        let requantized = !source.lossless || source.bits_per_sample != Some(output.valid_bits as u32);
        let remixed = source.channels != output.channels;
        let widened = source.bits_per_sample.is_some_and(|b| b <= output.valid_bits as u32);
        let lossless_end_to_end = source.lossless && output.lossless() && !resampled && !remixed && widened;
        Self { source, output, resampled, requantized, remixed, lossless_end_to_end }
    }
}
