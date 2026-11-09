# raspair

Turntable line-in on a Raspberry Pi, streamed to AirPlay 2 speakers, written in Rust.

```
turntable ──► ADC (I2S / USB) ──► vinyl-bridge-pi ──► AirPlay 2 ──► HomePod / stereo pair
```

| Path | What |
|---|---|
| [`apps/pi4-host`](apps/pi4-host) | `vinyl-bridge-pi`: capture, signal gate, recording, HTTP control API, Buildroot appliance image |
| [`apps/airplay-rs`](apps/airplay-rs) | `airplay-core` AirPlay 2 sender library and the `airplay` CLI |

Extracted with history from `pmdroid/vinyl-repo` (`feat/vinyl-management`), where the ESP32 firmware and the Android app still live.

## Development tools

Install [mise](https://mise.jdx.dev/getting-started.html) to use the Rust, Node.js and Bun versions pinned in [`mise.toml`](mise.toml). From the repository root:

```sh
mise trust
mise install
```

Use `mise exec --` to run commands with the pinned tools.

On Raspberry Pi OS and Ubuntu, install the native build dependencies:

```sh
sudo apt-get install --no-install-recommends build-essential pkg-config libasound2-dev libssl-dev
```

## Quick start

```sh
cd apps/pi4-host
mise exec -- cargo run -- --http-bind 127.0.0.1:8080
curl -s http://127.0.0.1:8080/api/status
```

Pi builds, the SD image and the HTTP API are documented in [`apps/pi4-host/README.md`](apps/pi4-host/README.md).

To run the React web UI, start from the repository root:

```sh
cd apps/web
mise exec -- bun install --frozen-lockfile
mise exec -- bun run dev
```
