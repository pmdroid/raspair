# raspair verification map

This directory is the maintained source for verifying what a user of the vinyl bridge can do. Read the index before driving, then use the matching feature file as the recipe. The user surface is the bridge's HTTP API, which the Android app and the embedded web UI (`GET /`, title `Vinyl Bridge`) call, plus what comes out the other end: speaker audio, the live capture stream and recorded files.

## Baseline preconditions

- Work from the repo root with `V=.claude/skills/verify-raspair/scripts/verify.sh`.
- Start a fresh run with `$V up` (realtime) or `$V up --mode buffered`. It prints `API=`, `EVIDENCE=` and the run env.
- `$V doctor` must end with `doctor: OK (run <RUN>)` and show `capture: "tone"`, the expected `mode`, and `git=` equal to the checkout you mean to verify.
- The tone (440 Hz left, 660 Hz right, −15 dBFS) is playing from the first second. For the bridge, a record is spinning.
- Exactly one AirPlay receiver exists, named `raspair-verify`, on the run's private network. No other speaker is reachable, and none may be added.
- Never drive an instance this run did not start. `$V runs` shows what exists.

## Driving conventions

- Call the API only through `$V api METHOD PATH [-d JSON]`. The web UI sends a body on every POST, so send at least `-d '{}'`. An empty POST body is rejected with `Failed to parse the request body as JSON`.
- Select speakers from what `GET /api/speakers` discovered, unchanged. Do not hand-write targets, override `model` or `tsid`, or set `force_open`. Those skip the selection a user makes.
- Wait on state, not time, wherever a state exists. For example, poll `GET /api/status` until `.airplay[0].note` leaves `connecting…`. Buffered mode arms for about 8 s before `live`.
- Save every response you rely on: `| tee "$EVIDENCE/<feature-id>-<step>.json"`.

## Proof and skip reporting

- A proof is the action, the API state after it, and the side effect: received audio, a file on disk, or a stream.
- Audio side effects are judged only by the oracle (`$V receiver-audio`, `$V stream-tap`, `$V check`). Report its JSON verdict and the WAV path.
- `"active": true` or `"note": "live …"` alone never proves audio reached a speaker.
- Record the feature ID, the entry point, the run ID, `MODE` and `GIT_REV` with each artifact.
- Report an unreachable path with the attempted command, the observed output and the unmet precondition.
- Do not report one entry point as verified through another. Realtime and buffered AirPlay are separate entry points.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behavior. It then uses exactly four H2 sections in this order.

1. `Sub-features` lists short IDs with one line for each behavior.
2. `How to get to it (user POV)` lists every user entry point.
3. `Driving it with verify.sh` starts with `Preconditions:` and uses labeled bullets that pair each user action with an exact command and observable result.
4. `Gotchas` lists traps that can waste or invalidate a verification run.

## Features

- [Stream to an AirPlay speaker](./airplay-stream.md) covers discovery, selection, the signal gate starting playback, received audio in buffered and realtime mode, volume and stop.
- [Record the record](./recording.md) covers manual start/stop, the file list, download and the recorded audio.
- [Live capture stream](./capture-stream.md) covers `/capture.wav` on the stream port and `/stream.wav` on the main port.
- [Settings and status](./settings-and-status.md) covers status, stats, runtime settings persistence and the web UI shell.
