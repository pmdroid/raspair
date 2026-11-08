# Record the record

The user starts and stops a recording from the app or web UI. The bridge writes a WAV of what the turntable plays into its record directory, lists it with its tag sidecar, and serves it for download.

## Sub-features

- `record-start` begins a session recording to a new `rec-<unix-ts>.wav`.
- `record-stop` finalizes the file and resets the recorder to idle.
- `record-files` lists finished recordings with size, mtime and tag status.
- `record-download` returns the WAV bytes for a listed file.
- `record-audio` makes the recorded audio match what was played: 24-bit 48 kHz stereo, same tone and level.

## How to get to it (user POV)

- Web UI: the Record start and stop buttons, and the recordings list with download links.
- Android app: Recordings screen (start, stop, download).
- HTTP: `POST /api/record/start`, `POST /api/record/stop`, `GET /api/record/files`, `GET /api/record/download?file=<name>`.

## Driving it with verify.sh

Preconditions:

- `$V doctor` reports OK.
- `$V api GET /api/record | jq .recording` is `false`.
- `source .verify/runs/$(cat .verify/current)/env` so `$HTTP_PORT` and `$RUN` are set, and `EVIDENCE=.verify/runs/$RUN/evidence`.

- **Start.** Press Record. Run `$V api POST /api/record/start -d '{}' | tee "$EVIDENCE/record-start.json" | jq -c '{recording, phase, path}'`. The response shows `recording: true`, `phase: "recording_session"` and `path: "/verify/rec/rec-<ts>.wav"`.
- **Let it play.** Wait 4 s. `$V api GET /api/record | jq .seconds` grows.
- **Stop.** Press Stop. Run `$V api POST /api/record/stop -d '{}' | tee "$EVIDENCE/record-stop.json" | jq -c '{recording, phase}'`. The response shows `recording: false` and `phase: "idle"`.
- **List.** Open the recordings list. Run `$V api GET /api/record/files | tee "$EVIDENCE/record-files.json" | jq -c '.files[] | {name, size, status}'`. The new `rec-<ts>.wav` is listed with a size of about 288 KB per second and `status: "skipped"` (music ID off).
- **Side effect on disk.** Run `ls -l .verify/runs/$RUN/rec/`. It contains `rec-<ts>.wav` and its `rec-<ts>.json` sidecar.
- **Download and listen.** Run `F=$(jq -r '.files[0].name' "$EVIDENCE/record-files.json"); curl -sS -o "$EVIDENCE/record-$F" "http://127.0.0.1:$HTTP_PORT/api/record/download?file=$F"` and then `$V check "$EVIDENCE/record-$F" --min-seconds 3`. The exit code is 0 with `"pass": true`, `bits: 24`, 440/660 Hz at about −15 dBFS, `silent_blocks` 0 and `bad_windows` 0.

## Gotchas

- `POST` without a body fails with `Failed to parse the request body as JSON: EOF while parsing`. Always send `-d '{}'` like the web UI.
- `.files[0]` is the newest file only when the directory was empty before. Pick the name from `record-start.json` `path` when several recordings exist.
- `down` deletes `.verify/runs/$RUN/rec/`. The downloaded copy in `$EVIDENCE` is the artifact that survives.
- The sidecar's `status` is `skipped` unless `music_id_enabled` is set with an AcoustID key and `fpcalc`, neither of which this harness provides.
