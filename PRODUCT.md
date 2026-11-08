# RaspAir

## Platform

web

## Stack

Pascal selected React for the web UI, with Oxfmt and Oxlint. The app lives in `apps/web` and builds static assets for the Rust host. The companion iOS and Android apps use Flutter.

## Users

People connecting a turntable or another line-level audio source to a Raspberry Pi, listening through network speakers, and keeping recordings for their music library.

## Product purpose

Install one executable on Raspberry Pi OS 64-bit and configure the device through its web UI. Select a HiFiBerry DAC/ADC or another supported input, configure capture and streaming independently, record FLAC on the Pi, organize songs, and listen from a phone.

## Operating context

The browser controls a local audio device. Listening, recording, and speaker connection are separate states. Setup includes the input board, capture format, speakers, and recording storage. Song review supports boundary corrections and metadata edits.

## Product language

Use sample rate, bit depth, and channels for capture. FLAC compression controls storage and encoding effort. Streaming quality belongs to each output. AirPlay realtime and buffered are separate transport modes. The original source session remains available when song boundaries change.

## Evidence

The approved product plan is the Linear project [RaspAir: single-binary Pi recorder and Flutter apps](https://linear.app/kyku/project/raspair-single-binary-pi-recorder-and-flutter-apps-76d3eab34f70). The existing embedded control page is `apps/pi4-host/static/index.html`. Device and audio contracts are defined by the project's implementation issues.

## Product principles

- Make listening and recording states easy to distinguish.
- Preserve the original audio while users organize songs.
- Show the effective setting after a change.
- Keep common actions direct and advanced controls available.
- Identify illustrative device data as a preview.
