//! 64-bit NTP timestamps (seconds since 1900 in the high 32 bits, fraction in
//! the low 32) used by the realtime sync/timing packets.

use std::time::{SystemTime, UNIX_EPOCH};

pub const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

pub fn ntp_now() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() + NTP_EPOCH_OFFSET;
    let frac = (now.subsec_nanos() as u64) * (1u64 << 32) / 1_000_000_000;
    (secs << 32) | frac
}
