//! BLE scanning and Apple Continuity parsing.

pub mod decrypt;
pub mod parser;
pub mod scanner;

pub use decrypt::{decrypt_for_device, matches_device};
pub use parser::decode_model_name;
