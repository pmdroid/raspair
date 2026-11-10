use airplay_core::audio::{AudioReader, MediaSource, Samples};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const REFERENCE_FRAMES: u64 = 4800;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn reference(frame: u64, channel: u64) -> i32 {
    ((((frame * 2 + channel) * 2_654_435_761) & 0xFFFF_FFFF) >> 8) as i32 - 0x80_0000
}

fn read_all(reader: &mut AudioReader) -> (Vec<i32>, Vec<f32>, u64) {
    let (mut ints, mut floats, mut next) = (Vec::new(), Vec::new(), None);
    while let Some(block) = reader.next_block().unwrap() {
        if let Some(expected) = next {
            assert_eq!(block.start_frame, expected);
        }
        assert!(block.frames > 0 && block.frames <= 16_384);
        next = Some(block.start_frame + block.frames as u64);
        match block.samples {
            Samples::Int { bits, data } => {
                assert_eq!(bits, 24);
                assert_eq!(data.len(), block.frames * reader.info().channels as usize);
                ints.extend(data);
            }
            Samples::Float(data) => floats.extend(data),
        }
    }
    (ints, floats, next.unwrap_or(0))
}

#[test]
fn lossless_24_bit_references_decode_sample_exactly() {
    let expected: Vec<i32> = (0..REFERENCE_FRAMES).flat_map(|n| [reference(n, 0), reference(n, 1)]).collect();
    for name in ["pcm24_48k_stereo.wav", "pcm24_48k_stereo.flac", "alac24_48k_stereo.m4a"] {
        let mut reader = AudioReader::open(fixture(name)).unwrap();
        let info = reader.info().clone();
        assert_eq!((info.sample_rate, info.channels, info.bits_per_sample), (48_000, 2, Some(24)), "{name}");
        assert_eq!(info.total_frames, Some(REFERENCE_FRAMES), "{name}");
        assert!(info.seekable, "{name}");
        let (ints, floats, end) = read_all(&mut reader);
        assert!(floats.is_empty(), "{name}");
        assert_eq!(end, REFERENCE_FRAMES, "{name}");
        assert!(ints == expected, "{name} differs from the 24-bit reference");
    }
}

#[test]
fn lossless_seek_reports_and_returns_the_exact_start_frame() {
    for name in ["pcm24_48k_stereo.wav", "pcm24_48k_stereo.flac", "alac24_48k_stereo.m4a"] {
        let mut reader = AudioReader::open(fixture(name)).unwrap();
        reader.next_block().unwrap().unwrap();
        let seek = reader.seek(3_001).unwrap();
        assert_eq!(seek.requested_frame, 3_001, "{name}");
        assert!(seek.seeked_frame <= 3_001, "{name}");
        assert_eq!(seek.start_frame, 3_001, "{name}");
        let block = reader.next_block().unwrap().unwrap();
        assert_eq!(block.start_frame, 3_001, "{name}");
        let Samples::Int { data, .. } = block.samples else { panic!("{name} is not integer") };
        assert_eq!(&data[..4], &[reference(3_001, 0), reference(3_001, 1), reference(3_002, 0), reference(3_002, 1)], "{name}");
    }
}

#[test]
fn lossy_fixtures_report_format_duration_and_tags() {
    for (name, rate, channels, priming) in [("tone_44k_stereo.mp3", 44_100, 2, 0), ("tone_48k_mono.m4a", 48_000, 1, 1024)] {
        let mut reader = AudioReader::open(fixture(name)).unwrap();
        let info = reader.info().clone();
        assert_eq!((info.sample_rate, info.channels), (rate, channels), "{name}");
        assert!(info.tags.contains(&("title".into(), "Fixture Tone".into())), "{name}: {:?}", info.tags);
        assert!(info.tags.contains(&("artist".into(), "raspair".into())), "{name}: {:?}", info.tags);
        let (ints, floats, end) = read_all(&mut reader);
        assert!(ints.is_empty(), "{name}");
        assert_eq!(floats.len() as u64, end * channels as u64, "{name}");
        assert_eq!(end, rate as u64 / 2 + priming, "{name}");
        assert_eq!(info.total_frames, Some(end), "{name}");
        let rms = (floats.iter().map(|s| s * s).sum::<f32>() / floats.len() as f32).sqrt();
        assert!((0.3..0.4).contains(&rms), "{name} rms {rms}");
    }
}

struct CountingSource {
    inner: Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
}

impl Read for CountingSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for CountingSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for CountingSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.inner.get_ref().len() as u64)
    }
}

fn wav_header(frames: u32) -> Vec<u8> {
    let data_len = frames * 6;
    let mut h = Vec::new();
    h.extend(b"RIFF");
    h.extend((36 + data_len).to_le_bytes());
    h.extend(b"WAVEfmt ");
    h.extend(16u32.to_le_bytes());
    h.extend(1u16.to_le_bytes());
    h.extend(2u16.to_le_bytes());
    h.extend(48_000u32.to_le_bytes());
    h.extend((48_000u32 * 6).to_le_bytes());
    h.extend(6u16.to_le_bytes());
    h.extend(24u16.to_le_bytes());
    h.extend(b"data");
    h.extend(data_len.to_le_bytes());
    h
}

#[test]
fn large_file_is_read_incrementally_and_can_stop_early() {
    let frames = 48_000 * 120;
    let mut bytes = wav_header(frames);
    bytes.resize(bytes.len() + frames as usize * 6, 0x11);
    let total = bytes.len() as u64;
    let read = Arc::new(AtomicU64::new(0));
    let source = CountingSource { inner: Cursor::new(bytes), read: read.clone() };
    let mut reader = AudioReader::from_source(Box::new(source), Some("wav")).unwrap();
    assert_eq!(reader.info().total_frames, Some(frames as u64));
    let mut decoded = 0;
    for _ in 0..8 {
        decoded += reader.next_block().unwrap().unwrap().frames;
    }
    drop(reader);
    let consumed = read.load(Ordering::Relaxed);
    assert!(decoded > 0);
    assert!(consumed < 1 << 20, "read {consumed} of {total} bytes for {decoded} frames");
}

#[test]
fn malformed_input_fails_or_ends_without_unbounded_work() {
    let garbage: Vec<u8> = (0..64 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let source = CountingSource { inner: Cursor::new(garbage), read: Arc::default() };
    assert!(AudioReader::from_source(Box::new(source), None).is_err());

    for name in ["pcm24_48k_stereo.flac", "alac24_48k_stereo.m4a", "tone_44k_stereo.mp3", "tone_48k_mono.m4a"] {
        let full = std::fs::read(fixture(name)).unwrap();
        for variant in 0..2 {
            let mut bytes = full.clone();
            if variant == 0 {
                bytes.truncate(full.len() / 2);
            } else {
                let start = full.len() / 3;
                bytes[start..start + 512].iter_mut().for_each(|b| *b ^= 0x5A);
            }
            let source = CountingSource { inner: Cursor::new(bytes), read: Arc::default() };
            let Ok(mut reader) = AudioReader::from_source(Box::new(source), None) else { continue };
            let mut blocks = 0;
            while let Ok(Some(_)) = reader.next_block() {
                blocks += 1;
                assert!(blocks < 10_000, "{name} variant {variant} did not terminate");
            }
        }
    }
}
