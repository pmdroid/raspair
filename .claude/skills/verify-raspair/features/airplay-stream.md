# Stream to an AirPlay speaker

The user picks an AirPlay speaker in the app or web UI. When the record plays (signal above the start threshold for the hold time), the bridge opens an AirPlay 2 session and the speaker plays the turntable in sync. Volume changes and Stop apply to the live session.

## Sub-features

- `airplay-discover` lists AirPlay receivers found over mDNS.
- `airplay-select` stores the chosen speaker as the target.
- `airplay-gate-start` opens the session by itself once the signal holds above the start threshold.
- `airplay-buffered` delivers audio over buffered AirPlay (type 103, PTP, PCM24).
- `airplay-realtime` delivers audio over realtime AirPlay (type 96, NTP, PCM16).
- `airplay-volume` changes the session volume.
- `airplay-stop` ends the session and the speaker goes quiet.

## How to get to it (user POV)

- Web UI: scan speakers, tick one, press Play (`POST /api/select`, then `POST /api/play`).
- Android app: select speakers (`POST /api/select`). The gate starts playback when the record plays.
- Web UI or app: the volume slider (`POST /api/volume`) and the Stop button (`POST /api/stop`).

## Driving it with verify.sh

Preconditions:

- `$V up --mode buffered` (or `$V up` for realtime) finished and `$V doctor` reports OK.
- `$V api GET /api/status | jq '.targets'` is `[]`.

- **Discover.** List speakers. Run `$V api GET /api/speakers | tee "$EVIDENCE/airplay-discover.json" | jq '.speakers[] | select(.name == "raspair-verify")'`. One entry appears with `"type": "airplay"`, `"model": "ShairportSync"`, `"port": 7000` and the receiver IP from the run env. Retry for up to 10 s right after `up`.
- **Select.** Choose the discovered receiver. Run `$V api POST /api/select -d "$($V api GET /api/speakers | jq -c '{speakers: [.speakers[] | select(.name == "raspair-verify")]}')" | tee "$EVIDENCE/airplay-select.json" | jq '{targets: [.targets[].name]}'`. `targets` is `["raspair-verify"]`.
- **Gate starts the session.** Do nothing else; the tone is the record. Poll `$V api GET /api/status | jq -c '{streaming, airplay}'` until the note leaves `connecting…`. `streaming` is `true`. The note is `live (buffered)` (or `live (realtime)`), or `error: …` naming the failing step. `$V logs bridge | grep airplay` shows `airplay: connected (…)` then `airplay: live sessions ready`.
- **Receiver plays it.** Run `$V receiver-audio 6`. The exit code is 0 and the JSON has `"pass": true`, channel 0 `expect_hz` 440 and channel 1 660, each `level_dbfs` near −15, and `silent_blocks` 0 and `bad_windows` 0. The WAV and JSON land in `$EVIDENCE/receiver-*.{wav,json}`.
- **PTP lock (buffered).** Run `$V receiver-ptp`. It prints `nqptp has a master clock`. Without a master the receiver never plays buffered audio, whatever the bridge status says.
- **Volume.** Run `$V api POST /api/volume -d '{"db": -20.0}' | jq .volume_db`. The response is `-20.0` and a follow-up `GET /api/status` shows `volume_db: -20.0`. The receiver ignores volume (`ignore_volume_control`), so level changes cannot be heard here. Prove only the API state and that `receiver-audio` still passes afterwards.
- **Stop.** Deselect and stop the way the app does. Run `$V api POST /api/select -d '{"speakers": []}'` and `$V api POST /api/stop -d '{}'`. Poll status until `streaming` is `false` and the note reads `stopped`. Then `stat -c %s .verify/runs/$RUN/receiver/audio.raw` does not grow over 3 s.

## Gotchas

- ShairportSync in AirPlay 2 mode rejects NTP-timed streams. Its log says `ntp stream handling is not implemented` and the SETUP returns 501. So `airplay-realtime` cannot be proven against this receiver. Report it unreachable with that log line. Realtime needs a HomePod, and real speakers are off limits.
- `error: connect not paired with raspair-verify yet — run the pair step first` means the sender chose persistent pairing for this receiver model. That is an AirPlay failure to report. Do not override `model` in the select body to get past it.
- `PTP did not lock before buffered anchor (Delay_Resp=0)` in the bridge log is normal for an nqptp receiver, which never sends Delay_Req. `$V receiver-ptp` is the real lock signal.
- The tone never stops, so the needle-lift path (silence for `audio_stop_timeout_sec` closes the session) cannot be driven with `capture_device = "tone"`.
- While the tone plays, the gate re-opens a session whenever `streaming` is false and a target is selected. Prove `airplay-stop` with the target deselected, or the session restarts within the 250 ms hold.
- `receiver-audio` slices only bytes written during its own window. Start it after the note says `live`, or it measures arming silence.
