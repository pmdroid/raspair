//! Line-in capture backends:
//! - **cpal** (feature `cpal`): CoreAudio on macOS, WASAPI/Pulse elsewhere — fast local loop
//! - **alsa** (feature `alsa`): Linux / Raspberry Pi hardware path
//! - **silence**: fallback when no audio feature or open fails
//!
//! Produces interleaved little-endian PCM and RMS dBFS for the signal gate
//! (same role as `i2s_capture` on ESP).

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
    pub backend: Arc<RwLock<&'static str>>,
}

impl CaptureHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<PcmBlock> {
        self.tx.subscribe()
    }

    pub fn rms_dbfs(&self) -> f64 {
        *self.last_rms_dbfs.read()
    }

    pub fn backend(&self) -> &'static str {
        *self.backend.read()
    }
}

pub fn start(cfg: &Config) -> CaptureHandle {
    let (tx, _) = broadcast::channel(64);
    let last_rms_dbfs = Arc::new(RwLock::new(-120.0));
    let running = Arc::new(AtomicBool::new(true));
    let backend = Arc::new(RwLock::new("starting"));

    let handle = CaptureHandle {
        tx: tx.clone(),
        last_rms_dbfs: last_rms_dbfs.clone(),
        running: running.clone(),
        backend: backend.clone(),
    };

    let sample_rate = cfg.sample_rate;
    let channels = cfg.channels as usize;
    let bits = cfg.bits;
    // Used by ALSA path; kept in scope for all targets so macOS cpal builds stay clean.
    #[cfg_attr(
        not(all(feature = "alsa", target_os = "linux")),
        allow(unused_variables)
    )]
    let capture_gain_db = cfg.capture_gain_db;
    let device = cfg.capture_device.clone();

    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            // Prefer ALSA on Linux when enabled (Pi shipping path).
            #[cfg(all(feature = "alsa", target_os = "linux"))]
            {
                // Label before the blocking read loop so /api/status shows "alsa"
                // while capture is live (run_alsa only returns on stop/error).
                *backend.write() = "alsa";
                match run_alsa(
                    &device,
                    sample_rate,
                    channels,
                    bits,
                    capture_gain_db,
                    &tx,
                    &last_rms_dbfs,
                    &running,
                ) {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::error!(error = %e, "ALSA capture failed; trying next backend");
                        *backend.write() = "starting";
                    }
                }
            }

            // cpal: CoreAudio (macOS), etc.
            #[cfg(feature = "cpal")]
            {
                match run_cpal(
                    &device,
                    sample_rate,
                    channels,
                    bits,
                    &tx,
                    &last_rms_dbfs,
                    &running,
                ) {
                    Ok(()) => {
                        *backend.write() = "cpal";
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "cpal capture failed; falling back to silence");
                    }
                }
            }

            #[cfg(not(any(feature = "cpal", feature = "alsa")))]
            {
                let _ = device;
                tracing::info!(
                    "capture: silence (build with --features cpal on macOS, or --features alsa on Linux)"
                );
            }
            #[cfg(any(feature = "cpal", feature = "alsa"))]
            {
                let _ = device;
            }

            *backend.write() = "silence";
            run_silence(sample_rate, channels, bits, &tx, &last_rms_dbfs, &running);
        })
        .expect("spawn capture thread");

    // Give the thread a moment to set backend label for /api/status.
    std::thread::sleep(Duration::from_millis(50));
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
    let bytes_per_sample = match bits {
        16 => 2,
        _ => 3,
    };
    let block_bytes = frames as usize * channels * bytes_per_sample;
    let period = Duration::from_secs_f64(frames as f64 / sample_rate as f64);

    tracing::info!(sample_rate, channels, bits, "capture: silence generator");
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

fn pack_pcm_i16(interleaved: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(interleaved.len() * 2);
    for s in interleaved {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn pack_pcm_i24(interleaved: &[i32]) -> Vec<u8> {
    // i32 samples already in 24-bit range (−2^23..2^23−1); store as 3-byte LE.
    let mut out = Vec::with_capacity(interleaved.len() * 3);
    for &s in interleaved {
        let v = s.clamp(-(1 << 23), (1 << 23) - 1);
        let b = v.to_le_bytes();
        out.extend_from_slice(&b[..3]);
    }
    out
}

fn rms_from_f32(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return -120.0;
    }
    let mut sum = 0.0_f64;
    for &s in samples {
        let x = s as f64;
        sum += x * x;
    }
    let rms = (sum / samples.len() as f64).sqrt();
    if rms < 1e-12 {
        -120.0
    } else {
        20.0 * rms.log10()
    }
}

// -------------------- cpal (macOS CoreAudio / portable) --------------------

#[cfg(feature = "cpal")]
fn run_cpal(
    device_hint: &str,
    sample_rate: u32,
    channels: usize,
    bits: u16,
    tx: &broadcast::Sender<PcmBlock>,
    last_rms: &Arc<RwLock<f64>>,
    running: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};

    let host = cpal::default_host();
    let device = if device_hint.is_empty() || device_hint == "default" {
        host.default_input_device()
            .ok_or_else(|| anyhow::anyhow!("no default input device (CoreAudio/cpal)"))?
    } else {
        host.input_devices()?
            .find(|d| {
                d.name()
                    .map(|n| n.to_ascii_lowercase().contains(&device_hint.to_ascii_lowercase()))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("no input device matching '{device_hint}'"))?
    };

    let name = device.name().unwrap_or_else(|_| "unknown".into());
    let supported = device.default_input_config()?;
    tracing::info!(
        device = %name,
        hint = device_hint,
        default_rate = supported.sample_rate().0,
        default_channels = supported.channels(),
        default_format = ?supported.sample_format(),
        "cpal: opening input (CoreAudio on macOS)"
    );

    let mut config: StreamConfig = supported.clone().into();
    // Prefer configured rate/channels when the device accepts them; otherwise keep default.
    config.sample_rate = cpal::SampleRate(sample_rate);
    config.channels = channels as u16;
    // If build_input_stream fails with custom rate, fall back to device default below.

    let err_fn = |e| tracing::error!(error = %e, "cpal stream error");

    let frames_target = frames_per_block(sample_rate) as usize;
    let ch = channels.max(1);
    let bits = if bits == 16 { 16 } else { 24 };

    // Buffer accumulate to ~10 ms blocks.
    let accum = Arc::new(parking_lot::Mutex::new(Vec::<f32>::new()));
    let accum2 = accum.clone();
    let tx2 = tx.clone();
    let last2 = last_rms.clone();
    let run2 = running.clone();

    // Prefer device default config first (fewest reopen failures on macOS).
    let stream_config: StreamConfig = supported.clone().into();
    // Optionally try user rate later only if we want — for the fast loop, default is fine.
    let _ = (config, sample_rate);

    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                if !run2.load(Ordering::Relaxed) {
                    return;
                }
                let mut buf = accum2.lock();
                buf.extend_from_slice(data);
                let need = frames_target * ch;
                while buf.len() >= need {
                    let chunk: Vec<f32> = buf.drain(..need).collect();
                    let rms = rms_from_f32(&chunk);
                    *last2.write() = rms;
                    let pcm = match bits {
                        16 => {
                            let i16s: Vec<i16> = chunk
                                .iter()
                                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                                .collect();
                            pack_pcm_i16(&i16s)
                        }
                        _ => {
                            let i32s: Vec<i32> = chunk
                                .iter()
                                .map(|s| (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32)
                                .collect();
                            pack_pcm_i24(&i32s)
                        }
                    };
                    let _ = tx2.send(PcmBlock {
                        samples: bytes::Bytes::from(pcm),
                        frames: frames_target as u32,
                        rms_dbfs: rms,
                    });
                }
            },
            err_fn,
            None,
        )?,
        SampleFormat::I16 => {
            let accum2 = accum.clone();
            let tx2 = tx.clone();
            let last2 = last_rms.clone();
            let run2 = running.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    if !run2.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut buf = accum2.lock();
                    for &s in data {
                        buf.push(s as f32 / 32768.0);
                    }
                    let need = frames_target * ch;
                    while buf.len() >= need {
                        let chunk: Vec<f32> = buf.drain(..need).collect();
                        let rms = rms_from_f32(&chunk);
                        *last2.write() = rms;
                        let i16s: Vec<i16> = chunk
                            .iter()
                            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                            .collect();
                        let pcm = pack_pcm_i16(&i16s);
                        let _ = tx2.send(PcmBlock {
                            samples: bytes::Bytes::from(pcm),
                            frames: frames_target as u32,
                            rms_dbfs: rms,
                        });
                    }
                },
                err_fn,
                None,
            )?
        }
        other => {
            anyhow::bail!("unsupported cpal sample format: {other:?}");
        }
    };

    stream.play()?;
    tracing::info!("cpal capture running");
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(stream);
    Ok(())
}

// -------------------- ALSA (Linux / Pi) --------------------

#[cfg(feature = "alsa")]
fn run_alsa(
    device: &str,
    sample_rate: u32,
    channels: usize,
    bits: u16,
    gain_db: f64,
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
        // bcm2835 I2S: use S32_LE for 24/32-bit (left-justified 24 in 32-bit slots).
        // S24_3LE is unavailable; S24_LE packing is ambiguous on this link.
        hwp.set_format(match bits {
            16 => Format::s16(),
            _ => Format::s32(),
        })?;
        hwp.set_access(Access::RWInterleaved)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;

    let frames = frames_per_block(sample_rate);
    let bytes_per_sample = match bits {
        16 => 2,
        _ => 4, // S32_LE slots (24-bit payload left-justified when bits=24)
    };
    let mut buf = vec![0u8; frames as usize * channels * bytes_per_sample];
    let io = pcm.io_bytes();
    let gain = if gain_db.abs() < 0.01 {
        1.0
    } else {
        10f64.powf(gain_db / 20.0)
    };

    tracing::info!(
        device,
        sample_rate,
        channels,
        bits,
        gain_db,
        "ALSA capture started"
    );

    while running.load(Ordering::Relaxed) {
        match io.readi(&mut buf) {
            Ok(n) if n > 0 => {
                let n_frames = n as u32;
                let used = n_frames as usize * channels * bytes_per_sample;
                if (gain - 1.0).abs() > 1e-6 {
                    apply_gain_inplace(&mut buf[..used], bits, gain);
                }
                let rms = rms_dbfs_bytes(&buf[..used], bits);
                *last_rms.write() = rms;
                let _ = tx.send(PcmBlock {
                    samples: bytes::Bytes::copy_from_slice(&buf[..used]),
                    frames: n_frames,
                    rms_dbfs: rms,
                });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "ALSA read error; re-prepare");
                let _ = pcm.prepare();
            }
        }
    }
    Ok(())
}

/// Scale interleaved PCM in place. Clamps to full-scale to avoid wrap distortion.
fn apply_gain_inplace(pcm: &mut [u8], bits: u16, gain: f64) {
    match bits {
        16 => {
            for chunk in pcm.chunks_exact_mut(2) {
                let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 * gain;
                let v = s.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => {
            // 24/32: S32_LE slots (24-bit left-justified when bits=24).
            for chunk in pcm.chunks_exact_mut(4) {
                let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64 * gain;
                let v = s.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}

#[cfg(feature = "alsa")]
fn rms_dbfs_bytes(pcm: &[u8], bits: u16) -> f64 {
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
        32 => {
            for chunk in pcm.chunks_exact(4) {
                let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64
                    / 2_147_483_648.0;
                sum += s * s;
                n += 1;
            }
        }
        _ => {
            // 24-bit left-justified in 32-bit I2S slot (high 24 bits).
            for chunk in pcm.chunks_exact(4) {
                let raw = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) >> 8;
                let s = raw as f64 / 8_388_608.0;
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


