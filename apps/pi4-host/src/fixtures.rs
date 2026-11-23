use crate::capture::PcmBlock;
use crate::config::Config;
use crate::record::{block_dbfs_wire, Recorder};

pub const RATE: u64 = 48_000;
pub const BLOCK: usize = 480;

const fn ms(v: u64) -> u64 {
    v * RATE / 1000
}

#[derive(Clone, Copy)]
pub enum Seg {
    Silence,
    Tone(f64, f64),
    FadeIn(f64, f64),
    FadeOut(f64, f64),
    Hum(f64),
    Noise(f64),
    Click(f64),
}

pub struct Case {
    pub name: &'static str,
    pub segs: &'static [(Seg, u64)],
    pub onset: Option<u64>,
    pub boundaries: &'static [u64],
}

use Seg::*;

pub static MANIFEST: &[Case] = &[
    Case { name: "clean_start", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "quiet_intro_40", segs: &[(Silence, ms(500)), (Tone(220.0, -40.0), ms(2000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "quiet_intro_50", segs: &[(Silence, ms(500)), (Tone(220.0, -50.0), ms(2000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "quiet_intro_60", segs: &[(Silence, ms(500)), (Tone(220.0, -60.0), ms(2000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "fade_in_slow", segs: &[(Silence, ms(500)), (FadeIn(330.0, -12.0), ms(4000)), (Tone(330.0, -12.0), ms(1000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "fade_in_fast", segs: &[(Silence, ms(500)), (FadeIn(330.0, -12.0), ms(300)), (Tone(330.0, -12.0), ms(2000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "click_before_music", segs: &[(Silence, ms(500)), (Click(-6.0), ms(500)), (Silence, ms(1000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(2000)), boundaries: &[] },
    Case { name: "click_only", segs: &[(Silence, ms(500)), (Click(-6.0), ms(1000)), (Silence, ms(2000))], onset: None, boundaries: &[] },
    Case { name: "hum_only", segs: &[(Hum(-50.0), ms(4000))], onset: None, boundaries: &[] },
    Case { name: "hum_then_music", segs: &[(Hum(-50.0), ms(1000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(1000)), boundaries: &[] },
    Case { name: "noise_floor_60", segs: &[(Noise(-60.0), ms(4000))], onset: None, boundaries: &[] },
    Case { name: "noise_floor_44", segs: &[(Noise(-44.0), ms(4000))], onset: None, boundaries: &[] },
    Case { name: "noise_then_music", segs: &[(Noise(-60.0), ms(1000)), (Tone(440.0, -12.0), ms(2000))], onset: Some(ms(1000)), boundaries: &[] },
    Case { name: "fade_out_end", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(2000)), (FadeOut(440.0, -12.0), ms(3000)), (Silence, ms(3000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "pause_inside_1s", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (Silence, ms(1000)), (Tone(440.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "pause_inside_2s", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (Silence, ms(2000)), (Tone(440.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[] },
    Case { name: "clean_gap_3s", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (Silence, ms(3000)), (Tone(330.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[ms(6500)] },
    Case { name: "noisy_gap_4s", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (Noise(-65.0), ms(4000)), (Tone(330.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[ms(7500)] },
    Case { name: "continuous_transition", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (Tone(330.0, -12.0), ms(3000))], onset: Some(ms(500)), boundaries: &[ms(3500)] },
    Case { name: "crossfade_transition", segs: &[(Silence, ms(500)), (Tone(440.0, -12.0), ms(3000)), (FadeOut(440.0, -12.0), ms(500)), (FadeIn(330.0, -12.0), ms(500)), (Tone(330.0, -12.0), ms(2500))], onset: Some(ms(500)), boundaries: &[ms(4000)] },
];

fn is_music(s: Seg) -> bool {
    matches!(s, Tone(..) | FadeIn(..) | FadeOut(..))
}

fn hash(n: u64) -> f64 {
    let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

fn amp(db: f64) -> f64 {
    32767.0 * 10f64.powf(db / 20.0)
}

fn sine(hz: f64, i: u64) -> f64 {
    (2.0 * std::f64::consts::PI * hz * (i as f64 + 0.5) / RATE as f64).sin()
}

impl Case {
    pub fn frames(&self) -> u64 {
        self.segs.iter().map(|s| s.1).sum()
    }

    pub fn render(&self) -> Vec<i16> {
        let mut out = Vec::with_capacity(self.frames() as usize * 2);
        let mut n = 0u64;
        for &(seg, len) in self.segs {
            for i in 0..len {
                let ramp = (i + 1) as f64 / len as f64;
                for ch in 0..2u64 {
                    let dither = |a: f64| a * 0.01 * hash(n * 2 + ch);
                    let v = match seg {
                        Silence => 0.0,
                        Tone(hz, db) => amp(db) * sine(hz, i) + dither(amp(db)),
                        FadeIn(hz, db) => ramp * (amp(db) * sine(hz, i) + dither(amp(db))),
                        FadeOut(hz, db) => (1.0 - ramp + 1.0 / len as f64) * (amp(db) * sine(hz, i) + dither(amp(db))),
                        Hum(db) => amp(db) * (0.8 * sine(50.0, n) + 0.2 * sine(150.0, n)),
                        Noise(db) => amp(db) * 1.7 * hash(n * 2 + ch),
                        Click(db) => if i < 2 { amp(db) } else { 0.0 },
                    };
                    out.push(v.round().clamp(-32768.0, 32767.0) as i16);
                }
                n += 1;
            }
        }
        out
    }
}

pub enum Fault {
    Drop(usize),
    Duplicate(usize),
}

pub fn replay(pcm: &[i16], faults: &[Fault], mut sink: impl FnMut(u64, &PcmBlock)) {
    for (idx, chunk) in pcm.chunks(BLOCK * 2).enumerate() {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        let block = PcmBlock { rms_dbfs: block_dbfs_wire(&bytes, 16), frames: (chunk.len() / 2) as u32, samples: bytes.into() };
        let copies = faults.iter().fold(1, |c, f| match f {
            Fault::Drop(i) if *i == idx => 0,
            Fault::Duplicate(i) if *i == idx => 2,
            _ => c,
        });
        for _ in 0..copies {
            sink((idx * BLOCK) as u64, &block);
        }
    }
}

pub fn first_audible(pcm: &[i16]) -> Option<u64> {
    pcm.chunks(2).position(|f| f.iter().any(|s| *s != 0)).map(|p| p as u64)
}

pub fn first_divergence(expected: &[i16], got: &[i16]) -> Option<u64> {
    let n = expected.len().max(got.len()) / 2;
    (0..n).find(|&f| expected.get(f * 2..f * 2 + 2) != got.get(f * 2..f * 2 + 2)).map(|f| f as u64)
}

#[derive(Debug, PartialEq)]
pub struct Baseline {
    pub gate_frame: Option<u64>,
    pub record_frame: Option<u64>,
    pub opening_lost: Option<u64>,
    pub tracks: usize,
}

pub fn fixed_threshold_baseline(case: &Case) -> Baseline {
    let dir = std::env::temp_dir().join(format!("pas1336-{}", uuid::Uuid::new_v4()));
    let defaults = Config::default();
    let cfg = Config { record_dir: dir.clone(), bits: 16, channels: 2, sample_rate: RATE as u32, record_auto_split: true, ..Config::default() };
    let rec = Recorder::new(&cfg);
    rec.start(None).expect("arm recorder");
    let hold = ms(defaults.audio_start_hold_ms);
    let (mut above, mut gate_frame, mut record_frame, mut track_start) = (None, None, None, None);
    replay(&case.render(), &[], |at, block| {
        let end = at + block.frames as u64;
        if block.rms_dbfs >= defaults.audio_start_threshold_db {
            let since = *above.get_or_insert(at);
            if gate_frame.is_none() && end - since >= hold {
                gate_frame = Some(end);
            }
        } else {
            above = None;
        }
        rec.write_pcm(&block.samples);
        if record_frame.is_none() && rec.status().phase == "recording_track" {
            record_frame = Some(end);
            track_start = Some(end - rec.status().bytes / 4);
        }
    });
    rec.stop().expect("stop recorder");
    let tracks = rec.list_files().expect("list tracks").len();
    let _ = std::fs::remove_dir_all(&dir);
    let opening_lost = case.onset.zip(track_start).map(|(o, s)| s.saturating_sub(o));
    Baseline { gate_frame, record_frame, opening_lost, tracks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_annotations_match_segment_layout() {
        assert_eq!(MANIFEST.len(), 20);
        for case in MANIFEST {
            let mut starts = Vec::new();
            let mut at = 0;
            for &(seg, len) in case.segs {
                starts.push((at, seg));
                at += len;
            }
            let music = starts.iter().find(|(_, s)| is_music(*s)).map(|(f, _)| *f);
            assert_eq!(music, case.onset, "{}", case.name);
            for b in case.boundaries {
                assert!(starts.iter().any(|(f, _)| f == b), "{} boundary {b}", case.name);
            }
            assert_eq!(case.render().len() as u64, case.frames() * 2);
            assert_eq!(case.render(), case.render(), "{} is deterministic", case.name);
        }
    }

    #[test]
    fn replay_without_faults_is_sample_exact() {
        for case in MANIFEST {
            let pcm = case.render();
            let mut got = Vec::new();
            let mut next = 0;
            replay(&pcm, &[], |at, b| {
                assert_eq!(at, next);
                next += b.frames as u64;
                got.extend(b.samples.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
            });
            assert_eq!(first_divergence(&pcm, &got), None, "{}", case.name);
            if case.name == "clean_start" {
                assert_eq!(first_audible(&got), case.onset);
            }
        }
    }

    #[test]
    fn known_first_audible_frame_detects_removed_or_duplicated_block() {
        let case = &MANIFEST[0];
        let onset = case.onset.unwrap();
        let pcm = case.render();
        assert_eq!(first_audible(&pcm), Some(onset));
        let onset_block = onset as usize / BLOCK;
        for fault in [Fault::Drop(onset_block - 3), Fault::Duplicate(onset_block - 3), Fault::Drop(onset_block), Fault::Duplicate(onset_block + 4)] {
            let mut got = Vec::new();
            replay(&pcm, &[fault], |_, b| got.extend(b.samples.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]))));
            let diverged = first_divergence(&pcm, &got).expect("fault must be visible");
            assert!(first_audible(&got) != Some(onset) || diverged >= onset);
        }
        let mut got = Vec::new();
        replay(&pcm, &[Fault::Drop(onset_block - 3)], |_, b| got.extend(b.samples.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]))));
        assert_eq!(first_audible(&got), Some(onset - BLOCK as u64));
        got.clear();
        replay(&pcm, &[Fault::Duplicate(onset_block - 3)], |_, b| got.extend(b.samples.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]))));
        assert_eq!(first_audible(&got), Some(onset + BLOCK as u64));
    }

    #[test]
    fn fixed_threshold_baseline_is_recorded() {
        let n = None;
        let expected = [
            (Some(36000), Some(38400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 1),
            (Some(132000), Some(134400), Some(91200), 1),
            (Some(132000), Some(134400), Some(91200), 1),
            (Some(41760), Some(44160), Some(960), 1),
            (Some(36480), Some(38880), Some(0), 1),
            (Some(108000), Some(110400), Some(0), 1),
            (n, n, n, 0),
            (n, n, n, 0),
            (Some(60000), Some(62400), Some(0), 1),
            (n, n, n, 0),
            (Some(12000), Some(14400), n, 1),
            (Some(60000), Some(62400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 2),
            (Some(36000), Some(38400), Some(0), 2),
            (Some(36000), Some(38400), Some(0), 1),
            (Some(36000), Some(38400), Some(0), 1),
        ];
        for (case, (gate_frame, record_frame, opening_lost, tracks)) in MANIFEST.iter().zip(expected) {
            assert_eq!(fixed_threshold_baseline(case), Baseline { gate_frame, record_frame, opening_lost, tracks }, "{}", case.name);
        }
    }
}
