//! Apple Accessory Protocol: L2CAP client, session setup and packets.
//!
//! [`Client::open`] brings a session up; [`decode`] reads what the device sends
//! on it. Each kind of packet has a module of its own, holding both the
//! request that asks for it and the parser that reads it.

pub mod battery;
pub mod client;
pub mod keys;
pub mod message;
pub mod noise;

pub use battery::{BatteryInfo, is_battery_packet, parse_battery_packet};
pub use client::Client;
pub use keys::{find_encryption_key, find_irk, parse_proximity_keys};
pub use message::{DecodeError, Message, decode};
pub use noise::{NoiseMode, is_noise_mode_packet, parse_noise_mode_packet};
