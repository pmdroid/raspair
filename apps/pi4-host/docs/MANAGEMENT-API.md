# Management API — smoke notes

Control plane for the Pi host (`vinyl-bridge-pi`). Same base as ESP
`p4-bridge` plus settings / collection / music ID / embedded web UI.

Default bind: `0.0.0.0:80` (use `--http-bind 127.0.0.1:8080` for local dev).

## Web UI

| URL | Notes |
|---|---|
| `http://<host>/` or `/ui` | Embedded management SPA (settings, speakers, record, collection) |
| `http://vinyl-bridge.local/` | mDNS on LAN |

Recordings live under **`record_dir`**:

| Loop | Path |
|---|---|
| macOS default | `./rec` |
| Pi / Buildroot | `/var/lib/vinyl-bridge/rec` |
| Config / settings | `GET /api/settings` → `record_dir` |

Runtime settings persist next to the record dir as `settings.json`
(e.g. `/var/lib/vinyl-bridge/settings.json`).

## Smoke (curl)

```bash
BASE=http://127.0.0.1:8080

# Status + parity
curl -sS "$BASE/api/status" | jq '{name,streaming,record,settings,parity}'

# Settings
curl -sS "$BASE/api/settings" | jq .
curl -sS -X PUT "$BASE/api/settings" \
  -H 'content-type: application/json' \
  -d '{"auto_record":true,"music_id_enabled":false,"volume_db":-28}' | jq .

# Speakers / stream connections
curl -sS "$BASE/api/speakers" | jq .
curl -sS -X POST "$BASE/api/select" -H 'content-type: application/json' \
  -d '{"speakers":[{"name":"Demo","ip":"192.168.1.10","port":7000,"type":"airplay"}]}' | jq .
curl -sS -X POST "$BASE/api/play" | jq .
curl -sS -X POST "$BASE/api/stop" | jq .
curl -sS -X POST "$BASE/api/volume" -H 'content-type: application/json' \
  -d '{"db":-25}' | jq .

# Record start / stop / list / download
curl -sS -X POST "$BASE/api/record/start" -H 'content-type: application/json' -d '{}' | jq .
curl -sS "$BASE/api/record" | jq .
# … play audio into capture, then:
curl -sS -X POST "$BASE/api/record/stop" | jq .
curl -sS "$BASE/api/record/files" | jq .
curl -sS -OJ "$BASE/api/record/download?file=rec-….wav"   # use real name from list

# Collection + identify (graceful if no fpcalc / key)
curl -sS "$BASE/api/collection" | jq .
curl -sS -X POST "$BASE/api/record/identify" \
  -H 'content-type: application/json' \
  -d '{"file":"rec-….wav"}' | jq .

# Manual tags into collection sidecar
curl -sS -X POST "$BASE/api/record/tag" \
  -H 'content-type: application/json' \
  -d '{"file":"rec-….wav","artist":"Artist","title":"Song","album":"LP"}' | jq .
```

## Silence begin/end

| Mode | How |
|---|---|
| **Session gate** | `audio_start_*` / `audio_stop_*` open/close AirPlay; with `auto_record` also start/stop WAV |
| **Track split** | `record_auto_split=true` → inter-track silence closes one WAV and opens the next (`rec-<ts>-t01.wav` …) |
| **Phone path** | Android `CaptureRecorder` already splits on silence over the capture tap |

## Android

Settings screen → `/api/settings`. Recordings → SD/session list, download, **Identify**.
Phone record continues to use local silence-split.

## Music ID setup

1. Install Chromaprint tools so `fpcalc` is on `PATH`  
   - macOS: `brew install chromaprint`  
   - Debian/Buildroot: package `chromaprint` / ship `fpcalc` binary  
   - Override: `FPCALC_PATH=/path/to/fpcalc`
2. Register an application at <https://acoustid.org/new-application>
3. Set the client API key via any of:
   - TOML: `acoustid_api_key = "…"` in `config.toml`
   - Env: `ACOUSTID_API_KEY=…`
   - Runtime: web UI Settings, Android Settings, or `PUT /api/settings`
4. Enable `music_id_enabled` (TOML, UI, or settings API)

Without key or `fpcalc`, recording still works; identify returns
`status: skipped` / `disabled` and writes a sidecar JSON.

Sidecars: `<wav>.json` next to the WAV (artist/title/album/mbid/score/status).

## Auto-record

When `auto_record` is true, the same energy gate used for stream open/close
also starts and stops session WAV recording (begin on sustained RMS above
start threshold; end after stop timeout of quiet). Manual start/stop still
works. Finalized files may be identified if music ID is ready.
