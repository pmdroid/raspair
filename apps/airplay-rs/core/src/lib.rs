//! airplay-core — an AirPlay 2 audio client ported from the project's Go and C
//! references. It implements device discovery, the HomeKit pairing handshakes
//! (transient pair-setup for HomePods, persistent pair-setup + pair-verify for
//! Apple TVs), the encrypted RTSP control channel, and both realtime (type 96,
//! NTP-timed) and buffered (type 103, PTP-timed) audio streams.
//!
//! The high-level entry point is [`client::Client`]; everything below it is
//! exposed so callers can drive the protocol directly.

pub mod audio;
pub mod client;
pub mod discovery;
pub mod pairing;
pub mod protocol;
pub mod rtsp;
pub mod stream;
pub mod timing;

pub use anyhow::{Error, Result};
pub use client::{
    begin_live_buffered_synchronized, begin_live_realtime_synchronized, BufferedAudioFormat,
    Client, StreamType,
};
pub use discovery::Device;
pub use timing::{PtpAnchor, PtpClock};
