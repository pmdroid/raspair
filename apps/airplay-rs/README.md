# airplay-rs

A Rust AirPlay 2 audio client, ported from this project's Go and C references.
Split into a reusable core library and a thin discover-and-play CLI.

```
apps/airplay-rs/
├── core/   airplay-core — pairing, RTSP, realtime + buffered audio, PTP/NTP timing, discovery
├── cli/    airplay      — `discover`, `pair`, `play`
└── docs/   ENTERTAINMENT_GROUPS.md — reverse-engineering notes on home-theater group streaming
```

> **Research note:** AirPlay 2 entertainment/home-theater groups (e.g. an Apple
> TV with HomePod speakers) distribute audio over **SSM multicast + a shared
> group key**, not the unicast fan-out this library does — which is why no OSS
> sender can target a grouped Apple TV. Captured/decrypted findings and an
> implementation plan are in [`docs/ENTERTAINMENT_GROUPS.md`](docs/ENTERTAINMENT_GROUPS.md).

## What's implemented

**Pairing modes**
- Transient pair-setup (HomePods) — fixed PIN `3939`, SRP session key is the
  channel secret, no identity saved.
- Persistent pair-setup (Apple TV) — SRP + Ed25519 controller identity saved as
  PKCS#8 PEM under `~/.airplay/<deviceid>.pem` (interchangeable with the Go
  reference).
- pair-verify — Curve25519 ECDH re-auth on the RTSP connection that then
  carries the encrypted session.

**Audio streams**
- Realtime (type 96, NTP-timed): UDP audio + per-second sync packets, a local
  timing responder, `/feedback` keepalive. 16-bit PCM.
- Buffered (type 103, PTP-timed): TCP framed audio, a software PTP grandmaster
  (UDP 319/320), SETPEERS + RECORD + FLUSHBUFFERED + SETRATEANCHORTIME anchor
  handshake. Lossy AAC-LC and **lossless** 24-bit PCM (LE/BE) and ALAC.

**Stereo pairs**
- Two receivers sharing a `tsid` are detected automatically and driven in
  lockstep: both speakers join one PTP timing group and share a single anchor,
  so the same rtpTime maps to the same network time on each. Works for realtime
  and every buffered format (including the lossless ones).

**Plus**: mDNS discovery with a JSON device cache, the AirPlay 2 channel/event
ChaCha20-Poly1305 cipher, stereo-pair detection, and ffmpeg-backed decoding for
FLAC/MP3/M4A/etc.

## CLI usage

```sh
cargo build --release        # binary at target/release/airplay

# list receivers on the LAN (cached in ~/.airplay/devices.json)
airplay discover

# pair an Apple TV (enter the on-screen PIN); HomePods need no pairing
airplay pair --device "Entertainment Room" --pin 1234

# play a file — "auto" picks buffered when the receiver supports it, else realtime
airplay play --device "Living Room" --file song.flac
airplay play --device "Living Room" --file song.flac --mode realtime

# buffered lossless (ALAC); --lossless is shorthand for --mode buffered --format alac48000-24
airplay play --device "Living Room" --file song.flac --lossless
airplay play --device "Living Room" --file song.flac --mode buffered --format pcm24-48

# a HomePod stereo pair is detected and driven in lockstep automatically;
# add --no-stereo to play only the named speaker
airplay play --device "Living Room" --file song.flac --lossless

# entertainment / home-theater group (Apple TV leader + HomePods):
# build the Doubletake fp-m3 helper first (see docs/ENTERTAINMENT_GROUPS.md)
airplay play --device "Entertainment Room" --file song.flac --group \
             --fairplay-helper ./bin/fp-m3
```

Formats: `aac44100`, `aac48000`, `pcm24-48`, `pcm24-48be`, `alac48000-24`.
Lossless = `pcm24-48` / `pcm24-48be` / `alac48000-24` (or just `--lossless`).

### Notes

- **Buffered mode binds privileged PTP ports 319/320** — run under `sudo` (or
  grant the capability), same as the Go/C references. Realtime mode does not.
- **ffmpeg** is required for non-WAV input and for AAC encoding.
- Tunables: `AP2_ANCHOR_LEAD_MS` (default 4000), `AP2_SEND_LEAD_MS` (default
  4000), `AIRPLAY_CONFIG_DIR` (default `~/.airplay`).

## Library usage

```rust
use airplay_core::{audio, client::Client, discovery};
use std::time::Duration;

let devices = discovery::load_or_discover("Living Room", Duration::from_secs(5), Duration::ZERO)?;
let dev = discovery::find_device(&devices, "Living Room").unwrap();

let mut client = Client::new();
client.connect(dev, "3939")?;            // transient pairing for HomePod
client.setup_realtime(48000, 2)?;
let (samples, sr, ch) = audio::load_audio("song.flac", 48000, 2)?;
client.play_pcm(&samples, sr, ch)?;
client.stop()?;
```

## Tests

```sh
cargo test
```

Covers the HomeKit SRP-6a/SHA-512 test vector, the channel-cipher round trip,
TLV fragment reassembly, buffered SSRC codec tagging, and binary-plist SETUP
serialization.
