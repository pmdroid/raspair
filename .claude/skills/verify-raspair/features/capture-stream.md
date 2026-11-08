# Live capture stream

The bridge serves the turntable live as an endless WAV, so the Android app, VLC or a Cast device can listen without AirPlay. It is the media URL handed to Chromecast.

## Sub-features

- `tap-stream-port` serves `/capture.wav` on the dedicated stream port (8088 in the container).
- `tap-main-port` serves the same stream at `/capture.wav`, `/stream` and `/stream.wav` on the main HTTP port.
- `tap-audio` delivers 24-bit 48 kHz stereo that matches the input.

## How to get to it (user POV)

- Android app: live listen (stream port).
- Any player: `http://<bridge>:8088/capture.wav` or `http://<bridge>/stream.wav`.
- `GET /api/status | jq .capture_stream` advertises the URLs.

## Driving it with verify.sh

Preconditions:

- `$V doctor` reports OK.
- `source .verify/runs/$(cat .verify/current)/env` and set `EVIDENCE=.verify/runs/$RUN/evidence`.

- **Advertised URLs.** Run `$V api GET /api/status | jq -c .capture_stream`. It shows `path: "/capture.wav"`, `sample_rate: 48000`, `bits: 24` and `channels: 2`.
- **Stream port.** Listen for 3 s. Run `$V stream-tap 3`. curl reports `Operation timed out after 3000 milliseconds` (the stream is endless; this is expected). The oracle exits 0 with `"pass": true`, `bits: 24`, 440/660 Hz at about −15 dBFS. The WAV and JSON land in `$EVIDENCE/capture-tap-*`.
- **Main port.** Run `curl -sS --max-time 3 "http://127.0.0.1:$HTTP_PORT/stream.wav" -o "$EVIDENCE/stream-main.wav"; $V check "$EVIDENCE/stream-main.wav"`. It passes the same way.

## Gotchas

- The WAV header announces an unbounded data size. Players and the oracle read until EOF, so cut the stream with `--max-time`, not a byte count.
- curl exit code 28 (timeout) is the normal end of a tap capture. Judge only the oracle's exit code.
