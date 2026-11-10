use airplay_core::audio::AudioInfo;
use airplay_core::client::AudioCodec;
use airplay_core::profile::{negotiate, preflight, ProfileTarget};
use airplay_core::protocol::{AuthRequirement, Route, StreamCandidate as S};
use airplay_core::{
    BufferedAudioFormat as F, FallbackPolicy, FallbackReason, OutputProfile, QualityReport, SessionError, SessionOptions,
    SessionProfile as P, SourceFormat, StreamType,
};

struct Fake {
    route: Route,
    reject: Vec<&'static str>,
    attempts: Vec<String>,
}

fn key(profile: &P) -> String {
    match profile {
        P::Realtime { sample_rate } => format!("realtime{sample_rate}"),
        P::Buffered(f) => f.name.to_string(),
    }
}

impl ProfileTarget for Fake {
    fn route(&self) -> &Route {
        &self.route
    }
    fn setup(&mut self, profile: &P) -> Result<(), SessionError> {
        let key = key(profile);
        self.attempts.push(key.clone());
        if self.reject.contains(&key.as_str()) {
            Err(SessionError::ReceiverRejected { status: 453 })
        } else {
            Ok(())
        }
    }
}

fn route(streams: &[S], rates: &[u32]) -> Route {
    Route { auth: AuthRequirement::HapTransient, streams: streams.to_vec(), realtime_rates: rates.to_vec() }
}

fn fake(streams: &[S], reject: &[&'static str]) -> Fake {
    Fake { route: route(streams, &[48000]), reject: reject.to_vec(), attempts: Vec::new() }
}

fn pcm() -> P {
    P::Buffered(F::pcm_48000_24_stereo(true))
}

#[test]
fn default_session_profile_is_strict_buffered_pcm_24_48() {
    let options = SessionOptions::default();
    assert!(matches!(options.fallback, FallbackPolicy::Forbid));
    let out = OutputProfile::of(&options.profile).unwrap();
    assert!(out.is_buffered_lossless_24_48());
    assert_eq!((out.codec, out.sample_rate, out.valid_bits, out.channels), (AudioCodec::Pcm, 48000, 24, 2));
    assert_eq!(out.pcm_bitrate(), 48000 * 24 * 2);
}

#[test]
fn buffered_pcm_and_alac_are_accepted_as_requested() {
    for (profile, codec) in [(pcm(), AudioCodec::Pcm), (P::Buffered(F::alac_48000_24_stereo()), AudioCodec::Alac)] {
        let mut target = fake(&[S::Buffered, S::Realtime], &[]);
        let (effective, selection) = negotiate(&mut target, &profile, &FallbackPolicy::Forbid).unwrap();
        assert_eq!(key(&effective), key(&profile));
        assert_eq!(selection.requested, selection.effective);
        assert_eq!(selection.fallback, None);
        assert_eq!(selection.effective.codec, codec);
        assert!(selection.effective.is_buffered_lossless_24_48());
        assert_eq!(selection.effective.pcm_bitrate(), 2_304_000);
        assert_eq!(target.attempts.len(), 1);
    }
}

#[test]
fn strict_requests_never_degrade() {
    let cases: [(&[S], &[&str], SessionError); 3] = [
        (&[S::Realtime], &[], SessionError::UnsupportedProfile),
        (&[], &[], SessionError::UnsupportedProfile),
        (&[S::Buffered, S::Realtime], &["pcm48000-24"], SessionError::ReceiverRejected { status: 453 }),
    ];
    for (streams, reject, expected) in cases {
        let mut target = fake(streams, reject);
        assert_eq!(negotiate(&mut target, &pcm(), &FallbackPolicy::Forbid).unwrap_err(), expected);
        assert!(target.attempts.iter().all(|a| a == "pcm48000-24"), "{:?}", target.attempts);
        assert_eq!(preflight(&route(streams, &[48000]), &pcm(), &FallbackPolicy::Forbid).is_ok(), reject.len() == 1);
    }
}

#[test]
fn permitted_fallback_reports_reason_and_effective_profile() {
    let policy = FallbackPolicy::Allow(vec![P::Buffered(F::alac_48000_24_stereo()), P::Realtime { sample_rate: 48000 }]);

    let mut target = fake(&[S::Buffered, S::Realtime], &["pcm48000-24"]);
    let (effective, selection) = negotiate(&mut target, &pcm(), &policy).unwrap();
    assert_eq!(key(&effective), "alac48000-24");
    assert_eq!(selection.fallback, Some(FallbackReason::SetupRejected { status: 453 }));
    assert!(selection.effective.is_buffered_lossless_24_48());

    let mut target = fake(&[S::Realtime], &[]);
    let (effective, selection) = negotiate(&mut target, &pcm(), &policy).unwrap();
    assert_eq!(key(&effective), "realtime48000");
    assert_eq!(selection.fallback, Some(FallbackReason::NotAdvertised));
    assert_eq!(selection.requested.valid_bits, 24);
    assert_eq!((selection.effective.transport, selection.effective.valid_bits), (StreamType::Realtime, 16));
    assert!(!selection.effective.is_buffered_lossless_24_48());
    assert_eq!(target.attempts, vec!["realtime48000"]);

    let mut target = fake(&[], &[]);
    assert_eq!(negotiate(&mut target, &pcm(), &policy).unwrap_err(), SessionError::UnsupportedProfile);
    assert!(target.attempts.is_empty());
}

#[test]
fn forbidden_or_unknown_profiles_are_rejected_before_setup() {
    let mut odd = F::pcm_48000_24_stereo(true);
    odd.bits_per_sample = 16;
    let invalid = [P::Buffered(odd), P::Realtime { sample_rate: 96000 }];
    for profile in &invalid {
        let mut target = fake(&[S::Buffered, S::Realtime], &[]);
        assert_eq!(negotiate(&mut target, profile, &FallbackPolicy::Forbid).unwrap_err(), SessionError::UnsupportedProfile);
        let policy = FallbackPolicy::Allow(vec![profile.clone()]);
        assert_eq!(negotiate(&mut target, &pcm(), &policy).unwrap_err(), SessionError::UnsupportedProfile);
        assert!(target.attempts.is_empty());
    }
    let aac = OutputProfile::of(&P::Buffered(F::aac_44100_stereo())).unwrap();
    assert!(!aac.lossless() && !aac.is_buffered_lossless_24_48());
}

fn info(codec: &str, sample_rate: u32, bits: Option<u32>) -> AudioInfo {
    AudioInfo {
        codec: codec.into(),
        sample_rate,
        channels: 2,
        bits_per_sample: bits,
        total_frames: None,
        delay_frames: 0,
        padding_frames: 0,
        seekable: true,
        tags: Vec::new(),
    }
}

#[test]
fn source_provenance_is_reported_separately_from_output() {
    let out = OutputProfile::of(&pcm()).unwrap();
    let native = QualityReport::new(SourceFormat::from(&info("flac", 48000, Some(24))), out);
    assert!(native.lossless_end_to_end && !native.resampled && !native.requantized);

    let cd = QualityReport::new(SourceFormat::from(&info("flac", 44100, Some(16))), out);
    assert!(cd.source.lossless && cd.resampled && cd.requantized && !cd.lossless_end_to_end);

    let mp3 = QualityReport::new(SourceFormat::from(&info("mp3", 48000, None)), out);
    assert!(!mp3.source.lossless && mp3.requantized && !mp3.lossless_end_to_end);
    assert!(mp3.output.lossless());
}

#[test]
fn realtime_rate_must_be_validated_for_the_receiver() {
    let rt44 = P::realtime(44100, 16).unwrap();
    let rt48 = P::realtime(48000, 16).unwrap();
    for (rates, ok, rejected) in [(&[48000][..], &rt48, &rt44), (&[44100][..], &rt44, &rt48)] {
        let mut target = Fake { route: route(&[S::Realtime], rates), reject: vec![], attempts: Vec::new() };
        let (_, selection) = negotiate(&mut target, ok, &FallbackPolicy::Forbid).unwrap();
        assert_eq!((selection.effective.sample_rate, selection.effective.valid_bits), (rates[0], 16));
        let rate = OutputProfile::of(rejected).unwrap().sample_rate;
        let error = negotiate(&mut target, rejected, &FallbackPolicy::Forbid).unwrap_err();
        assert_eq!(error, SessionError::RealtimeRate { requested: rate });
        assert!(error.to_string().contains(&format!("{rate} Hz")));
        assert_eq!(preflight(&target.route, rejected, &FallbackPolicy::Forbid).unwrap_err(), error);
        assert_eq!(target.attempts.len(), 1);

        let policy = FallbackPolicy::Allow(vec![ok.clone()]);
        let (effective, selection) = negotiate(&mut target, rejected, &policy).unwrap();
        assert_eq!(key(&effective), key(ok));
        assert_eq!(selection.fallback, Some(FallbackReason::RateNotValidated { sample_rate: rate }));
    }
}

#[test]
fn realtime_24_bit_requests_get_an_actionable_error() {
    let error = P::realtime(48000, 24).unwrap_err();
    assert_eq!(error, SessionError::RealtimeBitDepth { requested: 24 });
    let text = error.to_string();
    assert!(text.contains("16-bit only") && text.contains("24-bit") && text.contains("buffered"), "{text}");
    assert_eq!(P::realtime(96000, 16).unwrap_err(), SessionError::RealtimeRate { requested: 96000 });
    assert!(matches!(P::realtime(48000, 16), Ok(P::Realtime { sample_rate: 48000 })));
}
