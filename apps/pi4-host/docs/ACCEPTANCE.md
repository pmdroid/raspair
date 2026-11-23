# Audio acceptance fixtures and supported deployment contract

Tracking issue: PAS-1336. This file records the release contract, the initial
budgets, the measures used by later detection and playback work, and the
fixed-threshold baseline measured before detection changes.

The fixtures exercise software paths only. A fixture run is not hardware
validation.

## Supported deployment contract

| Item | Contract |
| -- | -- |
| Board | Raspberry Pi 4 |
| OS | Raspberry Pi OS Lite 64-bit. The same release must also boot the current 64-bit Pi OS image at release time. |
| Recorded per acceptance run | Image file name and `sha256sum` of the image, `uname -r`, `cat /etc/os-release`, Pi model from `/proc/device-tree/model` |
| Application | One downloadable `vinyl-bridge-pi` executable with the web UI and application codecs embedded |
| Host contract | Kernel drivers (ALSA, I2S overlays) and stock OS services. No extra runtime such as Node or Bun on the Pi. |
| Phone apps | New Flutter apps for iOS and Android in `apps/mobile` |

Out of scope for this release: Buildroot images, ESP firmware, Apple TV
entertainment groups, remote Internet access, and replacing the old mobile
apps. PAS-14 and PAS-15 remain related work. `STATUS.md` describes the older
Buildroot appliance and is not the release contract.

## Initial budgets

| Budget | Value |
| -- | -- |
| Capture history | 10 s (configurable 2–30 s) |
| Opening guard | 100 ms before the detected onset |
| Receiver setup deadline | 20 s |
| History and startup buffers | 64 MiB total |
| Automatic metadata lookup | Off until enabled |

## Measures

Each measure is reported on its own. None is derived from another.

| Measure | Definition | Source |
| -- | -- | -- |
| Detection reaction | Frames from the annotated onset to the frame at which detection fires | Fixtures and hardware |
| Receiver readiness | Time from detection to the receiver accepting audio | Hardware only |
| Audible latency | Time from a source frame entering capture to the same frame being heard on the speaker | Hardware only |
| Boundary error | Signed frames between an annotated song boundary and the detected boundary; a missed or extra boundary is counted separately | Fixtures and hardware |

Opening loss, the number of frames between the annotated onset and the first
frame kept in a recording or sent to a receiver, is a pass/fail check: it must
be 0.

## Fixtures

`src/fixtures.rs` (test-only) contains:

- `MANIFEST`: 20 synthetic cases at 48 kHz, 16-bit stereo. Each case lists
  its segments, the annotated onset frame and song-boundary frames. Cases cover
  quiet intros, fades, clicks, hum, noise floors, pauses inside songs, clean
  and noisy gaps, and continuous and crossfaded transitions.
- `Case::render`: deterministic PCM. Audible segments carry a seeded dither,
  so every block is distinct and a removed or repeated block is visible.
- `replay`: feeds 10 ms `PcmBlock`s with their source frame positions to any
  capture consumer without real-time sleeps. It can drop or duplicate a block.
- `first_audible` and `first_divergence`: continuity checks against the
  reference PCM.
- `fixed_threshold_baseline`: replays a case into `Recorder::write_pcm` with
  auto-split enabled, and models the `session::apply_gate` start rule
  (`audio_start_threshold_db`, `audio_start_hold_ms`) on a frame clock.

Run with `cargo test -p vinyl-bridge-pi fixtures`.

## Fixed-threshold baseline

Measured on main e5bb100 with default thresholds (gate start −45 dBFS for
250 ms; recorder track start −45 dBFS for 300 ms, stop −55 dBFS for 2.5 s,
400 ms preroll). Frames are at 48 kHz. The test
`fixed_threshold_baseline_is_recorded` pins these values, so a detection change
must update them on purpose.

| Case | Onset | Gate fires | Recorder starts | Opening lost | Tracks / expected |
| -- | -- | -- | -- | -- | -- |
| clean_start | 24000 | 36000 | 38400 | 0 | 1 / 1 |
| quiet_intro_40 | 24000 | 36000 | 38400 | 0 | 1 / 1 |
| quiet_intro_50 | 24000 | 132000 | 134400 | 91200 | 1 / 1 |
| quiet_intro_60 | 24000 | 132000 | 134400 | 91200 | 1 / 1 |
| fade_in_slow | 24000 | 41760 | 44160 | 960 | 1 / 1 |
| fade_in_fast | 24000 | 36480 | 38880 | 0 | 1 / 1 |
| click_before_music | 96000 | 108000 | 110400 | 0 | 1 / 1 |
| click_only | – | – | – | – | 0 / 0 |
| hum_only | – | – | – | – | 0 / 0 |
| hum_then_music | 48000 | 60000 | 62400 | 0 | 1 / 1 |
| noise_floor_60 | – | – | – | – | 0 / 0 |
| noise_floor_44 | – | 12000 | 14400 | – | 1 / 0 |
| noise_then_music | 48000 | 60000 | 62400 | 0 | 1 / 1 |
| fade_out_end | 24000 | 36000 | 38400 | 0 | 1 / 1 |
| pause_inside_1s | 24000 | 36000 | 38400 | 0 | 1 / 1 |
| pause_inside_2s | 24000 | 36000 | 38400 | 0 | 1 / 1 |
| clean_gap_3s | 24000 | 36000 | 38400 | 0 | 2 / 2 |
| noisy_gap_4s | 24000 | 36000 | 38400 | 0 | 2 / 2 |
| continuous_transition | 24000 | 36000 | 38400 | 0 | 1 / 2 |
| crossfade_transition | 24000 | 36000 | 38400 | 0 | 1 / 2 |

Findings:

- Detection reaction is 250 ms (gate) and 300 ms (recorder) for a clean onset.
- Quiet intros below −45 dBFS delay detection by 2.25 s, and the 400 ms
  preroll loses 1.9 s of the opening. A slow fade loses 20 ms.
- A −44 dBFS noise floor opens the gate and starts a false recording.
- Continuous and crossfaded transitions are not split. Silence gaps of 3 s or
  more are split.

## Hardware benchmark specification

To be run later on the supported Pi 4 image with a HiFiBerry ADC.

- Synthetic set: the 20 `MANIFEST` cases, played through the analog input.
- Real set: at least 30 permissioned real transitions (recordings the owner
  has the right to use), including quiet intros, live crowd noise, crossfades
  and vinyl run-in grooves. Each item has an annotation with source file,
  sha256, sample rate, onset frame and boundary frames, in the same shape as
  `Case`.
- Each run records the image sha256, `uname -r`, board, ADC profile and input
  gain, and reports detection reaction, receiver readiness, audible latency and
  boundary error as separate columns.

## Reuse of earlier work

Commits f3ca1c6 and 857899f (tone source and end-to-end verify scripts) exist
only on `feat/e2e-tone-receiver` in pmdroid/raspair#1. On 2026-11-23 that PR
was open and conflicting with main, so it is a reference for the hardware
benchmark and was not merged here.
