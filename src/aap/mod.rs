//! Apple Accessory Protocol: L2CAP client and packet parsers.

pub mod battery;
pub mod client;
pub mod keys;
pub mod noise;

pub use battery::{BatteryInfo, is_battery_packet, parse_battery_packet};
pub use client::Client;
pub use keys::{find_encryption_key, is_key_packet, parse_proximity_keys};
pub use noise::{NoiseMode, is_noise_mode_packet, parse_noise_mode_packet};
