//! RTSP/1.0 client, request/response model, and the AirPlay SETUP body
//! builders and response parsers.

pub mod client;
pub mod request;
pub mod setup;

pub use client::Client;
pub use request::{Method, Request, Response};
pub use setup::*;
