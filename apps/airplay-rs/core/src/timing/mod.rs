//! PTPv2 software grandmaster used to anchor the buffered audio timeline.

pub mod ptp;

pub use ptp::{PtpAnchor, PtpClock};
