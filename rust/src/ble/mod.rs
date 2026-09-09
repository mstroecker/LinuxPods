//! BLE scanning and Apple Continuity parsing.

pub mod decrypt;
pub mod parser;

pub use decrypt::{DecryptError, decrypt_proximity_payload};
pub use parser::{
    ParseError, PodSide, ProximityData, decode_battery, decode_color, decode_connection_state,
    decode_model_name, parse_proximity_data,
};
