//! Apple Accessory Protocol: L2CAP client and packet parsers.

pub mod battery;
pub mod client;
pub mod keys;

pub use battery::{BatteryInfo, is_battery_packet, parse_battery_packet};
pub use client::Client;
pub use keys::{find_encryption_key, is_key_packet, parse_proximity_keys};
