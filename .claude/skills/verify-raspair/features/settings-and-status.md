# Settings and status

The user sees what the bridge is doing (device name, input level, streaming, speakers, recorder) and changes runtime settings such as gate thresholds and auto-record. The settings persist across restarts.

## Sub-features

- `status-read` reports name, version, capture backend, input level, targets and per-speaker session notes.
- `stats-read` reports uptime, capture block count and session counters.
- `settings-read` returns the runtime settings.
- `settings-write` applies a partial update and persists it to `settings.json` next to the record dir.
- `web-ui` serves the management page.

## How to get to it (user POV)

- Web UI at `/` (title `Vinyl Bridge`): status header, Settings panel with a Save button.
- Android app: home status and the Settings screen.
- HTTP: `GET /api/status`, `GET /api/stats`, `GET|PUT /api/settings`.

## Driving it with verify.sh

Preconditions:

- `$V doctor` reports OK.
- `source .verify/runs/$(cat .verify/current)/env` and set `EVIDENCE=.verify/runs/$RUN/evidence`.

- **Status.** Run `$V api GET /api/status | tee "$EVIDENCE/status.json" | jq -c '{name, version, capture: .parity.capture, rms_dbfs, streaming}'`. It shows `name: "raspair-verify-bridge"`, `capture: "tone"` and `rms_dbfs` about −15.
- **Stats.** Run `$V api GET /api/stats | jq -c '{uptime_s, capture_blocks}'` twice, 2 s apart. `capture_blocks` grows by about 200 (10 ms blocks).
- **Read settings.** Run `$V api GET /api/settings | jq -c '{audio_start_threshold_db, auto_record, volume_db}'`. The defaults are −45.0, false and −30.0.
- **Write settings.** Save a new threshold. Run `$V api PUT /api/settings -d '{"audio_start_threshold_db": -40.0}' | jq .audio_start_threshold_db`. The response is `-40.0`.
- **Persisted.** Read the file the bridge wrote. Run `jq .audio_start_threshold_db .verify/runs/$RUN/settings.json`. It shows `-40.0`. Then restart the bridge with `$V restart-bridge` and `source` the env again. `$V api GET /api/settings | jq .audio_start_threshold_db` still shows `-40.0`.
- **Web UI.** Run `curl -s "http://127.0.0.1:$HTTP_PORT/" | grep -o '<title>[^<]*'`. It prints `<title>Vinyl Bridge`.

## Gotchas

- `settings.json` lives in the parent of `record_dir`, which is the run dir (`/verify` in the container). `down` deletes it, so copy it into `$EVIDENCE` if it is part of the proof.
- `POST /api/settings` is an alias of `PUT`. Both take partial JSON.
- A plain `docker restart` gives the bridge new random host ports, and the run env goes stale (`doctor` then fails with `Failed to connect`). Use `$V restart-bridge`, which rewrites `HTTP_PORT` and `STREAM_PORT` in the env and waits until the API answers.
