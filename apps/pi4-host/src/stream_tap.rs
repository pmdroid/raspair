//! Live capture stream (ESP `capture_tap` parity).
//!
//! - TCP **:8088** `GET /capture.wav` — Android `CaptureRecorder` port
//! - Also served on the main HTTP API as `/capture.wav`, `/stream`, `/stream.wav`
//!
//! Output format is fixed for phone compatibility:
//! **48 kHz, stereo, 24-bit PCM** (3 bytes/sample), streaming WAV header
//! (RIFF/data sizes = 0xFFFFFFFF). Wire capture may be 96 kHz S32; we pack and
//! optionally decimate.

use crate::capture::CaptureHandle;
use crate::config::Config;
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Android `CaptureRecorder.CAPTURE_PORT`.
pub const DEFAULT_STREAM_PORT: u16 = 8088;

/// Stream format (matches ESP capture_tap + Android).
pub const STREAM_RATE: u32 = 48_000;
pub const STREAM_CHANNELS: u16 = 2;
pub const STREAM_BITS: u16 = 24;
const STREAM_BYTES_PER_SAMPLE: usize = 3;

#[derive(Clone)]
pub struct StreamParams {
    pub wire_rate: u32,
    pub wire_bits: u16,
    pub channels: u16,
}

impl StreamParams {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            wire_rate: cfg.sample_rate,
            wire_bits: cfg.bits,
            channels: cfg.channels,
        }
    }
}

/// Spawn TCP server on `cfg.stream_port` (default 8088).
pub fn start_tcp_server(capture: CaptureHandle, cfg: &Config) {
    let port = cfg.stream_port;
    let params = StreamParams::from_config(cfg);
    tokio::spawn(async move {
        if let Err(e) = run_tcp_server(capture, port, params).await {
            tracing::error!(error = %e, port, "capture stream TCP server failed");
        }
    });
}

async fn run_tcp_server(
    capture: CaptureHandle,
    port: u16,
    params: StreamParams,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        port,
        rate = STREAM_RATE,
        bits = STREAM_BITS,
        "capture stream server up (GET /capture.wav)"
    );
    loop {
        let (sock, peer) = listener.accept().await?;
        let cap = capture.clone();
        let p = params.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_tcp_client(sock, cap, p).await {
                tracing::debug!(error = %e, %peer, "capture stream client closed");
            }
        });
    }
}

async fn serve_tcp_client(
    mut sock: TcpStream,
    capture: CaptureHandle,
    params: StreamParams,
) -> anyhow::Result<()> {
    // Drain HTTP request (path ignored; any GET is fine).
    let mut req = [0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut req)).await;

    sock.set_nodelay(true).ok();

    let preamble = http_and_wav_preamble();
    sock.write_all(&preamble).await?;

    let mut rx = capture.subscribe();
    loop {
        match rx.recv().await {
            Ok(block) => {
                let pcm = wire_to_stream_pcm(
                    &block.samples,
                    params.wire_bits,
                    params.wire_rate,
                    params.channels,
                );
                if pcm.is_empty() {
                    continue;
                }
                sock.write_all(&pcm).await?;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(lagged = n, "capture stream lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

/// Axum handler body for `/capture.wav` / `/stream` on the main HTTP port.
pub fn axum_live_response(capture: CaptureHandle, params: StreamParams) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let preamble = Bytes::from(http_body_wav_only());
        if tx.send(Ok(preamble)).await.is_err() {
            return;
        }
        let mut sub = capture.subscribe();
        loop {
            match sub.recv().await {
                Ok(block) => {
                    let pcm = wire_to_stream_pcm(
                        &block.samples,
                        params.wire_bits,
                        params.wire_rate,
                        params.channels,
                    );
                    if pcm.is_empty() {
                        continue;
                    }
                    if tx.send(Ok(Bytes::from(pcm))).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "close")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn http_and_wav_preamble() -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(
        b"HTTP/1.0 200 OK\r\n\
Content-Type: audio/wav\r\n\
Cache-Control: no-cache\r\n\
Connection: close\r\n\
\r\n",
    );
    out.extend_from_slice(&streaming_wav_header(
        STREAM_RATE,
        STREAM_CHANNELS,
        STREAM_BITS,
    ));
    out
}

fn http_body_wav_only() -> Vec<u8> {
    streaming_wav_header(STREAM_RATE, STREAM_CHANNELS, STREAM_BITS).to_vec()
}

/// 44-byte PCM WAV header with unknown length (0xFFFFFFFF sizes).
pub fn streaming_wav_header(rate: u32, channels: u16, bits: u16) -> [u8; 44] {
    let block_align = channels * (bits / 8);
    let byte_rate = rate * u32::from(block_align);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());
    h
}

/// Convert capture wire PCM → 48 kHz / 24-bit packed stereo for the tap stream.
pub fn wire_to_stream_pcm(
    wire: &[u8],
    wire_bits: u16,
    wire_rate: u32,
    channels: u16,
) -> Vec<u8> {
    if wire.is_empty() || channels == 0 {
        return Vec::new();
    }
    let packed = pack_to_24le(wire, wire_bits);
    let frame = channels as usize * STREAM_BYTES_PER_SAMPLE;
    if frame == 0 || packed.len() < frame {
        return Vec::new();
    }
    // Align
    let usable = packed.len() - (packed.len() % frame);
    let packed = &packed[..usable];

    if wire_rate == STREAM_RATE || wire_rate == 0 {
        return packed.to_vec();
    }
    if wire_rate == STREAM_RATE * 2 {
        // Simple 96 kHz → 48 kHz: keep every other frame.
        let mut out = Vec::with_capacity(packed.len() / 2);
        let mut i = 0;
        let mut keep = true;
        while i + frame <= packed.len() {
            if keep {
                out.extend_from_slice(&packed[i..i + frame]);
            }
            keep = !keep;
            i += frame;
        }
        return out;
    }
    // Unknown rate: pass packed as-is (WAV header still says 48 k — best effort).
    tracing::trace!(wire_rate, "stream: unexpected capture rate; no resample");
    packed.to_vec()
}

fn pack_to_24le(wire: &[u8], wire_bits: u16) -> Vec<u8> {
    match wire_bits {
        16 => {
            let mut out = Vec::with_capacity(wire.len() / 2 * 3);
            for c in wire.chunks_exact(2) {
                let s = i16::from_le_bytes([c[0], c[1]]) as i32;
                // Expand to 24-bit (left-ish in 24: shift up 8).
                let v = s << 8;
                let b = v.to_le_bytes();
                out.extend_from_slice(&b[0..3]);
            }
            out
        }
        24 | 32 => {
            // Left-justified 24 in 32-bit slot (Pi I2S) → pack high 24 bits.
            let mut out = Vec::with_capacity(wire.len() / 4 * 3);
            for c in wire.chunks_exact(4) {
                let x = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let v = x >> 8;
                let b = v.to_le_bytes();
                out.extend_from_slice(&b[0..3]);
            }
            out
        }
        _ => {
            // Assume already 3-byte frames if divisible by 3.
            if wire.len() % 3 == 0 {
                wire.to_vec()
            } else {
                Vec::new()
            }
        }
    }
}
