//! BLE scanning and Apple Continuity parsing.

pub mod decrypt;
pub mod parser;
pub mod scanner;

pub use decrypt::decrypt_proximity_payload;
pub use parser::decode_model_name;
