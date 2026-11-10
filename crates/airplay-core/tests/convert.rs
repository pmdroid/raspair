use airplay_core::audio::convert::{OUTPUT_RATES, SOURCE_RATES};
use airplay_core::audio::{encode_alac_escape_frame, Converter, OutputFormat, PcmEncoder, Samples, SourceFormat};

const OUT_48_24: OutputFormat = OutputFormat { sample_rate: 48_000, bits: 24 };

fn run(conv: &mut Converter, samples: &[i32], bits: u32, block_frames: usize) -> Vec<f32> {
    let ch = conv.source().channels as usize;
    let mut out = Vec::new();
    for block in samples.chunks(block_frames * ch) {
        out.extend_from_slice(conv.push(&Samples::Int { bits, data: block.to_vec() }).unwrap());
    }
    out.extend_from_slice(conv.finish().unwrap());
    out
}

fn run_f32(conv: &mut Converter, samples: &[f32]) -> Vec<f32> {
    let mut out = conv.push(&Samples::Float(samples.to_vec())).unwrap().to_vec();
    out.extend_from_slice(conv.finish().unwrap());
    out
}

fn tone(rate: u32, freq: f64, frames: usize, amp: f64) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let v = (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()) as f32;
            [v, v]
        })
        .collect()
}

fn rms(s: &[f32]) -> f64 {
    (s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
}

#[test]
fn bypass_preserves_integer_edges_exactly() {
    let edges = [-8_388_608, 8_388_607, 0, 1, -1, 0x40_0000, -0x40_0000, 0x12_3456, -0x65_4321, 2, 8_388_606, -8_388_607];
    let mut conv = Converter::new(SourceFormat::int(48_000, 2, 24, 24), OUT_48_24, 1.0, 0).unwrap();
    let plan = conv.plan();
    assert!(plan.exact && !plan.resample && !plan.dither && plan.delay_frames == 0);
    let out = run(&mut conv, &edges, 24, 3);
    assert_eq!(out.len(), edges.len());

    let pcm = PcmEncoder::new(48_000, 2, 24, true).encode(&out);
    let expected: Vec<u8> = edges.iter().flat_map(|&v| (v as u32).to_le_bytes()[..3].to_vec()).collect();
    assert_eq!(pcm, expected);

    let alac = encode_alac_escape_frame(&out, edges.len() / 2);
    let bit = |i: usize| ((alac[i / 8] >> (7 - i % 8)) & 1) as u32;
    for (n, &e) in edges.iter().enumerate() {
        let v = (0..24).fold(0, |a, k| a << 1 | bit(23 + n * 24 + k));
        assert_eq!(v, e as u32 & 0xFF_FFFF);
    }

    let mut capture = Converter::new(SourceFormat::int(48_000, 2, 32, 24), OUT_48_24, 1.0, 0).unwrap();
    let wire: Vec<u8> = edges.iter().flat_map(|&v| ((v as u32) << 8).to_le_bytes()).collect();
    assert_eq!(capture.push_le_bytes(&wire).unwrap(), &out[..]);
}

#[test]
fn mono_is_duplicated_and_16_bit_widened_exactly() {
    let mut conv = Converter::new(SourceFormat::int(48_000, 1, 16, 16), OUT_48_24, 1.0, 0).unwrap();
    assert!(conv.plan().exact);
    let out = run(&mut conv, &[-32_768, 32_767, 5], 16, 2);
    let pcm = PcmEncoder::new(48_000, 2, 24, false).encode(&out);
    assert_eq!(pcm, [0x80, 0, 0, 0x80, 0, 0, 0x7F, 0xFF, 0, 0x7F, 0xFF, 0, 0, 5, 0, 0, 5, 0]);
}

#[test]
fn impulses_keep_position_channel_order_duration_and_tail() {
    for &src in &SOURCE_RATES {
        for &dst in &OUTPUT_RATES {
            if src == dst {
                continue;
            }
            let frames = src as usize / 4;
            let marks = [frames / 3, frames - 8];
            let mut input = vec![0i32; frames * 2];
            input[marks[0] * 2] = 4_000_000;
            input[marks[1] * 2 + 1] = 4_000_000;
            let mut conv = Converter::new(SourceFormat::int(src, 2, 24, 24), OutputFormat { sample_rate: dst, bits: 24 }, 1.0, 0)
                .unwrap();
            assert!(conv.plan().resample && conv.plan().delay_frames > 0);
            let out = run(&mut conv, &input, 24, 1000);
            assert_eq!(out.len() as u64 / 2, conv.output_frames_for(frames as u64), "{src}->{dst}");
            assert_eq!(conv.output_frames(), out.len() as u64 / 2);
            for (ch, &mark) in marks.iter().enumerate() {
                let peak = (0..out.len() / 2).max_by(|&a, &b| out[a * 2 + ch].abs().total_cmp(&out[b * 2 + ch].abs())).unwrap();
                let expected = mark as f64 * dst as f64 / src as f64;
                assert!((peak as f64 - expected).abs() <= 1.0, "{src}->{dst} ch{ch}: {peak} vs {expected}");
                assert!(conv.source_frame_at(peak as u64).abs_diff(mark as u64) <= (src / dst + 1) as u64);
                let other = out.iter().skip(1 - ch).step_by(2).skip(peak.saturating_sub(4)).take(9);
                assert!(other.map(|v| v.abs()).fold(0.0, f32::max) < 1e-3, "{src}->{dst} crosstalk");
            }
        }
    }
}

#[test]
fn sweep_passes_band_and_aliases_are_rejected() {
    for &(src, dst) in &[(96_000, 48_000), (192_000, 44_100), (44_100, 48_000), (192_000, 48_000)] {
        let frames = src as usize;
        let (f0, f1) = (100.0f64, 18_000.0f64);
        let sweep: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let t = i as f64 / src as f64;
                let k = (f1 / f0).ln();
                let v = (0.5 * (2.0 * std::f64::consts::PI * f0 * (k * t).exp_m1() / k).sin()) as f32;
                [v, -v]
            })
            .collect();
        let out_fmt = OutputFormat { sample_rate: dst, bits: 24 };
        let mut conv = Converter::new(SourceFormat::float(src, 2), out_fmt, 1.0, 0).unwrap();
        let out = run_f32(&mut conv, &sweep);
        let edge = dst as usize / 20 * 2;
        let ratio = rms(&out[edge..out.len() - edge]) / rms(&sweep[edge..sweep.len() - edge]);
        assert!((ratio - 1.0).abs() < 0.02, "{src}->{dst} passband {ratio}");
        assert!(out.chunks(2).all(|f| f[0] == -f[1]));

        if src > dst {
            let alias = tone(src, dst as f64 * 0.5 + 6_000.0, frames, 0.5);
            let mut conv = Converter::new(SourceFormat::float(src, 2), out_fmt, 1.0, 0).unwrap();
            let out = run_f32(&mut conv, &alias);
            let level = 20.0 * (rms(&out[edge..out.len() - edge]) / rms(&alias)).log10();
            assert!(level < -90.0, "{src}->{dst} alias {level} dB");
        }
    }
}

#[test]
fn block_sizes_produce_identical_output_with_deterministic_dither() {
    let frames = 30_000;
    let input: Vec<i32> = (0..frames * 2).map(|i| ((i as i64 * 2_654_435_761) % 1_000_000_000) as i32).collect();
    for (src, out_fmt, valid) in [
        (96_000, OUT_48_24, 32),
        (44_100, OutputFormat { sample_rate: 48_000, bits: 16 }, 24),
        (48_000, OutputFormat { sample_rate: 48_000, bits: 16 }, 24),
    ] {
        let data: Vec<i32> = input.iter().map(|&v| v >> (32 - valid)).collect();
        let outputs: Vec<Vec<f32>> = [1, 7, 352, 1024, 4096, frames]
            .iter()
            .map(|&block| {
                let mut conv = Converter::new(SourceFormat::int(src, 2, 32, valid), out_fmt, 0.8, 42).unwrap();
                assert!(conv.plan().dither);
                run(&mut conv, &data, valid, block)
            })
            .collect();
        assert!(outputs.iter().all(|o| o == &outputs[0]), "{src} valid {valid}");
        let scale = (1u32 << (out_fmt.bits - 1)) as f32;
        assert!(outputs[0].iter().all(|v| (v * scale).fract() == 0.0));
        let mut other = Converter::new(SourceFormat::int(src, 2, 32, valid), out_fmt, 0.8, 43).unwrap();
        assert_ne!(run(&mut other, &data, valid, 1024), outputs[0]);
    }
}

#[test]
fn dither_only_when_reducing_valid_bits() {
    let out16 = OutputFormat { sample_rate: 48_000, bits: 16 };
    assert!(!Converter::new(SourceFormat::int(44_100, 2, 24, 24), OUT_48_24, 1.0, 0).unwrap().plan().dither);
    assert!(!Converter::new(SourceFormat::float(48_000, 2), OUT_48_24, 1.0, 0).unwrap().plan().dither);
    assert!(!Converter::new(SourceFormat::int(48_000, 2, 32, 16), out16, 1.0, 0).unwrap().plan().dither);
    assert!(Converter::new(SourceFormat::int(48_000, 2, 32, 24), out16, 1.0, 0).unwrap().plan().dither);
    assert!(Converter::new(SourceFormat::float(96_000, 1), out16, 1.0, 0).unwrap().plan().dither);
    assert!(!Converter::new(SourceFormat::int(48_000, 2, 24, 24), OUT_48_24, 0.5, 0).unwrap().plan().exact);
}

#[test]
fn invalid_formats_fail_at_construction() {
    let bad = [
        (SourceFormat::int(22_050, 2, 16, 16), OUT_48_24),
        (SourceFormat::int(88_200, 2, 24, 24), OUT_48_24),
        (SourceFormat::int(48_000, 0, 24, 24), OUT_48_24),
        (SourceFormat::int(48_000, 6, 24, 24), OUT_48_24),
        (SourceFormat::int(48_000, 2, 24, 32), OUT_48_24),
        (SourceFormat::int(48_000, 2, 8, 8), OUT_48_24),
        (SourceFormat::int(48_000, 2, 24, 0), OUT_48_24),
        (SourceFormat::int(48_000, 2, 24, 24), OutputFormat { sample_rate: 96_000, bits: 24 }),
        (SourceFormat::int(48_000, 2, 24, 24), OutputFormat { sample_rate: 48_000, bits: 32 }),
    ];
    for (src, out) in bad {
        assert!(Converter::new(src, out, 1.0, 0).is_err(), "{src:?} -> {out:?}");
    }
    assert!(Converter::new(SourceFormat::int(48_000, 2, 24, 24), OUT_48_24, f32::NAN, 0).is_err());

    let mut conv = Converter::new(SourceFormat::int(48_000, 2, 24, 24), OUT_48_24, 1.0, 0).unwrap();
    assert!(conv.push(&Samples::Int { bits: 16, data: vec![0, 0] }).is_err());
    assert!(conv.push(&Samples::Float(vec![0.0, 0.0])).is_err());
    assert!(conv.push(&Samples::Int { bits: 24, data: vec![0] }).is_err());
    assert!(conv.push_le_bytes(&[0; 4]).is_err());
    conv.finish().unwrap();
    assert!(conv.push(&Samples::Int { bits: 24, data: vec![0, 0] }).is_err());
}
