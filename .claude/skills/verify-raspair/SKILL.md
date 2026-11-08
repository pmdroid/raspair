---
name: verify-raspair
description: Drive the raspair vinyl bridge (vinyl-bridge-pi + airplay-core) end to end on this machine and capture proof. A built-in tone stands in for the turntable, the bridge runs its real HTTP API, and a silent ShairportSync AirPlay 2 receiver in an isolated rootless-Docker network records what arrives. Use it to verify AirPlay streaming (realtime or buffered), recording, the live capture stream, settings and status, and any new bridge feature before claiming it works. Never streams to real speakers.
---

# verify-raspair

Proves bridge behaviour the way a user exercises it: needle down (tone), pick a speaker over the HTTP API the Android app and web UI use, and check what the speaker actually received. Everything runs in containers on a private Docker network, so nothing touches the LAN speakers.

All commands run from the repo root. The helper is `.claude/skills/verify-raspair/scripts/verify.sh`; below it is `V`:

```sh
V=.claude/skills/verify-raspair/scripts/verify.sh
```

## Setup facts

- Docker on this machine is **rootless** (`docker context ls` shows `rootless *`). Container IPs are not reachable from the host, so the bridge cannot run on the host and talk to the receiver. Both run as containers on the network `raspair-verify-<RUN>`. Only the bridge's HTTP port (8080) and stream port (8088) are published, on random `127.0.0.1` ports.
- **Receiver**: `mikebrady/shairport-sync:latest` (AirPlay 2, with NQPTP). Its pipe backend writes S16_LE 48 kHz stereo to `.verify/runs/<RUN>/receiver/audio.raw`. `ignore_volume_control = "yes"`, so received level does not depend on the bridge volume.
- **Bridge**: `apps/pi4-host` built with `--no-default-features` inside `rust:1-bookworm`. Build caches live in the volumes `raspair-verify-target` and `raspair-verify-cargo`. Each run copies the binary into its run dir, so a later build can't swap it under a running bridge.
- **Turntable stand-in**: `capture_device = "tone"`. It sends 440 Hz on the left channel and 660 Hz on the right at −15 dBFS RMS. The signal gate (start −45 dBFS, 250 ms hold) sees a record playing from the first second.
- **Never** select real speakers (the Living Room HomePods, Entertainment Room, ZimaBoard2, …) or run the bridge on the host network. The user chose no live streaming to real receivers. The isolated network enforces that only while you stay inside it.
- Host tools: `docker`, `curl`, `jq`, `python3`.

## Launch

```sh
$V up                    # realtime AirPlay (type 96, NTP, PCM16), the default mode
$V up --mode buffered    # buffered AirPlay (type 103, PTP, PCM24)
```

`up` builds the bridge (minutes the first time, then incremental) and starts the receiver. It waits until the receiver listens on `:7000`, then starts the bridge and waits until `GET /api/status` answers with `parity.capture == "tone"`. It prints the run's `env` (`RUN`, `MODE`, `HTTP_PORT`, `STREAM_PORT`, `RECEIVER_IP`, `GIT_REV`, `BINARY_SHA`) plus `API=` and `EVIDENCE=`. The run becomes current (`.verify/current`). Address another run with `RASPAIR_RUN=<RUN> $V …`.

The AirPlay mode is config-only. To verify the other mode, run `$V down` and then `$V up --mode <other>`. Use `--mode buffered` for AirPlay proofs. ShairportSync rejects NTP-timed (realtime) streams with `ntp stream handling is not implemented`, so realtime can only be reported as unreachable here.

The build output goes to `.verify/runs/<RUN>/build.log`. On a failed build `up` prints its tail and stops. After any failed `up`, run `$V down`: the env is written before the first container starts, so `down` always finds what to remove.

## Doctor

```sh
$V doctor
```

This is read-only. It checks four things:

- Both containers are running and labelled with this run.
- The API answers as `raspair-verify-bridge` with tone capture. It prints the status summary: streaming, rms, targets, and per-speaker `active`/`note`.
- The on-disk binary hash matches the one built for this run, and it shows which git revision the run was built from versus the checkout now.
- The receiver is listening on `:7000`. It also prints how many bytes the receiver has written so far.

It exits 1 on any mismatch. Run it first and again whenever something looks off. If the checkout moved since `up`, the running bridge is stale: `down` and `up` again.

## Drive

The feature map in [`features/`](features/README.md) holds the recipes. The core loop:

```sh
$V api GET /api/speakers | jq '.speakers[] | select(.name == "raspair-verify")'
$V api POST /api/select -d "$($V api GET /api/speakers | jq -c '{speakers: [.speakers[] | select(.name == "raspair-verify")]}')"
$V api GET /api/status | jq '.airplay'
$V receiver-audio 5
```

- `$V api METHOD PATH [curl args]` calls the bridge API with a JSON content type. It is the same HTTP surface the Android app and the web UI (`GET /`) use. Send a body on every POST/PUT, at least `-d '{}'`, as the web UI does. An empty body fails with `Failed to parse the request body as JSON`.
- `$V receiver-ptp` (buffered mode) reports whether the receiver's nqptp accepted the bridge as PTP master. Without one, the receiver plays nothing even while the bridge says `live (buffered)`.
- `$V receiver-audio SECONDS` slices exactly what the receiver played during the next SECONDS into `evidence/receiver-<time>.wav`. It runs the oracle and writes `evidence/receiver-<time>.json`. Exit 0 means the audio passed.
- `$V stream-tap SECONDS` records the live `/capture.wav` from the stream port into evidence and checks it the same way.
- `$V check FILE.wav [--level-db N --expect 440,660 --min-seconds N]` runs the oracle on any WAV, for example a downloaded recording.
- `$V logs bridge|receiver` prints container logs. Bridge lines to look for: `airplay: connected (realtime pcm16-48)`, `airplay: live sessions ready`, `sessions started`.

### Audio oracle (`scripts/check_audio.py`)

The oracle passes only when all of these hold:

- At least `--min-seconds` (default 1) of audio is present.
- No silent 10 ms block sits inside the audio.
- Every half-overlapping 100 ms window is dominated by the expected tone on its channel: 440 Hz left, 660 Hz right.
- Spectral purity is at least 0.9 in every window. A dropped or duplicated 352-frame packet anywhere shows up as purity of about 0.57–0.77.
- Each channel's level is within ±3 dB of −15 dBFS.

It prints JSON with `pass`, `reasons`, per-channel `level_dbfs` and `purity_min`, `silent_blocks` and `bad_windows`. Swapped channels, byte-order bugs, dropouts, resampling glitches and wrong gain all fail it.

## Evidence

Everything for a run goes to `.verify/runs/<RUN>/evidence/` (gitignored), and it survives `down`:

- `receiver-*.wav` / `.json`: what the AirPlay receiver played, plus the oracle verdict.
- `capture-tap-*.wav` / `.json`: the live capture stream plus its verdict.
- `bridge.log`, `receiver.log`, `env`: written by `down`.
- Your own captures: save API responses with `| tee .verify/runs/$RUN/evidence/<feature>-<step>.json`.

Proof standards:

- Drive the user path: `/api/select`, `/api/play|stop|volume`, `/api/record/*`, `/api/settings`, and the tone gate. Do not use config `[[targets]]` or `force_open` to skip the selection the user makes.
- Capture the action and the resulting state. That means the API response after the call, the status that follows, and the receiver audio or files that prove the side effect.
- Side effects count more than status flags. `"active": true` without a passing `receiver-audio` is not proof that AirPlay works.
- The receiver is a real AirPlay 2 implementation. It is not a mock of the bridge. HomePod-only behaviour cannot be proven here: stereo pairs via `tsid`, HomePod volume curves and FairPlay groups. Report those as not verified and don't infer them from a passing ShairportSync run.

## Cleanup

```sh
$V down
```

This saves both container logs and `env` into evidence. It removes only the containers and network labelled `raspair-verify=<RUN>`, and deletes the run's scratch: receiver raw audio, recordings dir, binary copy and `settings.json`. It keeps `evidence/` and prints its path. Run it after every attempt, including failed ones. `$V runs` lists runs, any containers still alive and their evidence paths.

Never `docker rm` by name pattern or kill processes by name. Other containers on this machine (agentbox and others) are not yours. The build-cache volumes are shared by all runs. Remove them only if the user asks: `docker volume rm raspair-verify-target raspair-verify-cargo`.

## Helpers

| Command | Does |
|---|---|
| `$V up [--mode realtime\|buffered]` | Build, start receiver + bridge, wait until ready, print env |
| `$V doctor` | Read-only health and identity check; exit 1 if not worth driving |
| `$V api METHOD PATH [curl args]` | Call the bridge API |
| `$V receiver-audio SECONDS` | Record and check what the AirPlay receiver plays |
| `$V stream-tap SECONDS` | Record and check `/capture.wav` |
| `$V check FILE.wav [opts]` | Run the audio oracle on a WAV |
| `$V receiver-ptp` | Buffered mode: does the receiver's nqptp have a PTP master |
| `$V restart-bridge` | Restart this run's bridge, refresh its ports in the env, wait until ready |
| `$V logs bridge\|receiver` | Container logs |
| `$V down` | Tear down this run, keep evidence |
| `$V runs` | List runs and evidence dirs |
