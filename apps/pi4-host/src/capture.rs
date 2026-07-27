//! Line-in capture: ALSA on Linux (feature `alsa`), silence generator otherwise.
//!
//! Produces interleaved PCM frames at the configured rate/bit depth and reports
//! a rough RMS in dBFS for the signal gate (same role as i2s_capture on ESP).

use crate::config::Config;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// One capture block: interleaved little-endian PCM.
#[derive(Clone)]
pub struct PcmBlock {
    pub samples: bytes::Bytes,
    /// Frames (per channel) in this block.
    pub frames: u32,
    pub rms_dbfs: f64,
}

#[derive(Clone)]
pub struct CaptureHandle {
    pub tx: broadcast::Sender<PcmBlock>,
    pub last_rms_dbfs: Arc<RwLock<f64>>,
    pub running: Arc<AtomicBool>,
}

impl CaptureHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<PcmBlock> {
        self.tx.subscribe()
    }

    pub fn rms_dbfs(&self) -> f64 {
        *self.last_rms_dbfs.read()
    }
}

pub fn start(cfg: &Config) -> CaptureHandle {
    let (tx, _) = broadcast::channel(64);
    let last_rms_dbfs = Arc::new(RwLock::new(-120.0));
    let running = Arc::new(AtomicBool::new(true));

    let handle = CaptureHandle {
        tx: tx.clone(),
        last_rms_dbfs: last_rms_dbfs.clone(),
        running: running.clone(),
    };

    let sample_rate = cfg.sample_rate;
    let channels = cfg.channels as usize;
    let bits = cfg.bits;
    let device = cfg.capture_device.clone();

    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            #[cfg(feature = "alsa")]
            {
                if let Err(e) = run_alsa(
                    &device,
                    sample_rate,
                    channels,
                    bits,
                    &tx,
                    &last_rms_dbfs,
                    &running,
                ) {
                    tracing::error!(error = %e, "ALSA capture failed; falling back to silence");
                    run_silence(sample_rate, channels, bits, &tx, &last_rms_dbfs, &running);
                }
            }
            #[cfg(not(feature = "alsa"))]
            {
                let _ = device;
                tracing::info!(
                    "capture: silence generator (build with --features alsa on Linux for real line-in)"
                );
                run_silence(sample_rate, channels, bits, &tx, &last_rms_dbfs, &running);
            }
        })
        .expect("spawn capture thread");

    handle
}

fn frames_per_block(sample_rate: u32) -> u32 {
    // ~10 ms blocks
    (sample_rate / 100).max(48)
}

fn run_silence(
    sample_rate: u32,
    channels: usize,
    bits: u16,
    tx: &broadcast::Sender<PcmBlock>,
    last_rms: &Arc<RwLock<f64>>,
    running: &Arc<AtomicBool>,
) {
    let frames = frames_per_block(sample_rate);
    let bytes_per_sample = (bits as usize / 8).max(2);
    let block_bytes = frames as usize * channels * bytes_per_sample;
    let period = Duration::from_secs_f64(frames as f64 / sample_rate as f64);

    while running.load(Ordering::Relaxed) {
        let samples = bytes::Bytes::from(vec![0u8; block_bytes]);
        let rms = -120.0_f64;
        *last_rms.write() = rms;
        let _ = tx.send(PcmBlock {
            samples,
            frames,
            rms_dbfs: rms,
        });
        std::thread::sleep(period);
    }
}

#[cfg(feature = "alsa")]
fn run_alsa(
    device: &str,
    sample_rate: u32,
    channels: usize,
    bits: u16,
    tx: &broadcast::Sender<PcmBlock>,
    last_rms: &Arc<RwLock<f64>>,
    running: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::{Direction, ValueOr};

    let pcm = PCM::new(device, Direction::Capture, false)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(channels as u32)?;
        hwp.set_rate(sample_rate, ValueOr::Nearest)?;
        hwp.set_format(match bits {
            16 => Format::s16(),
            _ => Format::s24_3(),
        })?;
        hwp.set_access(Access::RWInterleaved)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;

    let frames = frames_per_block(sample_rate);
    let bytes_per_sample = match bits {
        16 => 2,
        _ => 3,
    };
    let mut buf = vec![0u8; frames as usize * channels * bytes_per_sample];
    let io = pcm.io_bytes();

    tracing::info!(
        device,
        sample_rate,
        channels,
        bits,
        "ALSA capture started"
    );

    while running.load(Ordering::Relaxed) {
        match io.readi(&mut buf) {
            Ok(n) if n > 0 => {
                let n_frames = n as u32;
                let used = n_frames as usize * channels * bytes_per_sample;
                let rms = rms_dbfs(&buf[..used], bits);
                *last_rms.write() = rms;
                let _ = tx.send(PcmBlock {
                    samples: bytes::Bytes::copy_from_slice(&buf[..used]),
                    frames: n_frames,
                    rms_dbfs: rms,
                });
            }
            Ok(_) => {}
            Err(e) => {
                // Recover from overrun
                if let Some(errno) = e.errno() {
                    if errno as i32 == libc::EPIPE {
                        let _ = pcm.prepare();
                        continue;
                    }
                }
                return Err(e.into());
            }
        }
    }
    Ok(())
}

#[cfg(feature = "alsa")]
fn rms_dbfs(pcm: &[u8], bits: u16) -> f64 {
    if pcm.is_empty() {
        return -120.0;
    }
    let mut sum = 0.0_f64;
    let mut n = 0_usize;
    match bits {
        16 => {
            for chunk in pcm.chunks_exact(2) {
                let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
                sum += s * s;
                n += 1;
            }
        }
        _ => {
            for chunk in pcm.chunks_exact(3) {
                let raw = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]) << 8 >> 8;
                let s = raw as f64 / 8_388_608.0; // 2^23
                sum += s * s;
                n += 1;
            }
        }
    }
    if n == 0 {
        return -120.0;
    }
    let rms = (sum / n as f64).sqrt();
    if rms < 1e-12 {
        -120.0
    } else {
        20.0 * rms.log10()
    }
}
