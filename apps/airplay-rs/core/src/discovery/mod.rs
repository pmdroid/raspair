//! mDNS discovery and a JSON device cache.

pub mod cache;
pub mod device;
pub mod discover;

pub use cache::load_or_discover;
pub use device::Device;
pub use discover::{discover, find_device};
