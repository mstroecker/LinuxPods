//! AAP proximity keys: the request for them, and the packet that carries them.
//!
//! Packet layout:
//!   0-1: header (04 00)   2-3: command (04 00)
//!   4:   key marker 0x31  5: unknown   6: key count
//! Then per key: [type] [unknown] [len] [unknown] [data...]

use crate::KEY_LEN;
use crate::aap::message::{self, HEADER};

/// Identity-Resolving Key / Encryption Key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Irk,
    EncKey,
    Unknown(u8),
}

impl From<u8> for KeyType {
    fn from(v: u8) -> Self {
        match v {
            0x01 => Self::Irk,
            0x04 => Self::EncKey,
            other => Self::Unknown(other),
        }
    }
}

impl From<KeyType> for u8 {
    fn from(t: KeyType) -> Self {
        match t {
            KeyType::Irk => 0x01,
            KeyType::EncKey => 0x04,
            KeyType::Unknown(other) => other,
        }
    }
}

impl std::fmt::Display for KeyType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Irk => write!(f, "IRK (Identity Resolving Key)"),
            Self::EncKey => write!(f, "ENC_KEY (Encryption Key)"),
            Self::Unknown(v) => write!(f, "UNKNOWN (0x{v:02X})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProximityKey {
    pub key_type: KeyType,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyParseError {
    #[error("packet too short (need at least 7 bytes, got {0})")]
    TooShort(usize),
    #[error("not a key packet")]
    NotKeyPacket,
    #[error("no keys in packet (key count = 0)")]
    NoKeys,
    #[error("suspicious key count: {0} (expected 1-10)")]
    SuspiciousCount(usize),
    #[error("packet too short for key {0} header")]
    TruncatedHeader(usize),
    #[error("packet too short for key {0} data")]
    TruncatedData(usize),
    #[error("no ENC_KEY in key packet")]
    NoEncryptionKey,
    #[error("ENC_KEY must be {KEY_LEN} bytes, got {0}")]
    BadEncryptionKeyLen(usize),
}

/// The key packet's opcode, the key marker.
pub const OPCODE: u8 = 0x31;
const KEY_MARKER: u8 = OPCODE;
const MAX_KEYS: usize = 10;

/// Requests the proximity pairing encryption keys (IRK and ENC_KEY).
pub const REQUEST: [u8; 8] = [
    HEADER[0], HEADER[1], HEADER[2], HEADER[3], 0x30, 0x00, 0x05, 0x00,
];

pub fn is_key_packet(packet: &[u8]) -> bool {
    packet.len() >= 7 && message::opcode(packet) == Some(KEY_MARKER)
}

pub fn parse_proximity_keys(packet: &[u8]) -> Result<Vec<ProximityKey>, KeyParseError> {
    if packet.len() < 7 {
        return Err(KeyParseError::TooShort(packet.len()));
    }
    if !is_key_packet(packet) {
        return Err(KeyParseError::NotKeyPacket);
    }
    read(packet)
}

/// [`parse_proximity_keys`] past its header check, for a packet
/// [`is_key_packet`] has accepted.
pub(super) fn read(packet: &[u8]) -> Result<Vec<ProximityKey>, KeyParseError> {
    let key_count = packet[6] as usize;
    if key_count == 0 {
        return Err(KeyParseError::NoKeys);
    }
    if key_count > MAX_KEYS {
        return Err(KeyParseError::SuspiciousCount(key_count));
    }

    let mut keys = Vec::with_capacity(key_count);
    let mut offset = 7;

    for i in 0..key_count {
        if offset + 3 >= packet.len() {
            return Err(KeyParseError::TruncatedHeader(i + 1));
        }

        let key_type = KeyType::from(packet[offset]);
        let key_length = packet[offset + 2] as usize;
        offset += 4; // skip 4-byte header

        if offset + key_length > packet.len() {
            return Err(KeyParseError::TruncatedData(i + 1));
        }

        keys.push(ProximityKey {
            key_type,
            data: packet[offset..offset + key_length].to_vec(),
        });
        offset += key_length;
    }

    Ok(keys)
}

/// The ENC_KEY is what actually decrypts BLE advertisements.
///
/// The length is checked here, not only at decryption: the key comes from the
/// device and is persisted, so one of any other length would replace a working
/// key on disk and then never decrypt anything.
pub fn find_encryption_key(keys: &[ProximityKey]) -> Result<[u8; KEY_LEN], KeyParseError> {
    let key = keys
        .iter()
        .find(|k| k.key_type == KeyType::EncKey)
        .ok_or(KeyParseError::NoEncryptionKey)?;
    key.data
        .as_slice()
        .try_into()
        .map_err(|_| KeyParseError::BadEncryptionKeyLen(key.data.len()))
}

/// How many bytes a key packet starts with ahead of its first key: the header,
/// the opcode, the byte after it and the key count.
pub const PREAMBLE_LEN: usize = 7;

/// A packet as hex, to print where it may be pasted into an issue, a key
/// packet's keys left out: of one, only its first [`PREAMBLE_LEN`] bytes go
/// out, and a note. The keys are the pair's IRK and ENC_KEY. Any packet with
/// the key opcode is cut short, whether it parses or not.
pub fn redacted_hex(packet: &[u8]) -> String {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if message::opcode(packet) == Some(KEY_MARKER) {
        let preamble = &packet[..packet.len().min(PREAMBLE_LEN)];
        format!("{} (keys redacted)", hex(preamble))
    } else {
        hex(packet)
    }
}

pub fn find_irk(keys: &[ProximityKey]) -> Option<&[u8]> {
    keys.iter()
        .find(|k| k.key_type == KeyType::Irk)
        .map(|k| k.data.as_slice())
}

/// Builds the packet [`parse_proximity_keys`] reads, keys in the order given.
/// The device sends the IRK, then the ENC_KEY.
///
/// # Panics
///
/// With more than 255 keys, or a key longer than 255 bytes: the count and
/// length bytes cannot hold them.
#[cfg(any(test, feature = "harness"))]
pub fn key_packet(keys: &[ProximityKey]) -> Vec<u8> {
    let count = u8::try_from(keys.len()).expect("at most 255 keys in a packet");
    let mut packet = HEADER.to_vec();
    packet.extend_from_slice(&[KEY_MARKER, 0x00, count]);
    for key in keys {
        let len = u8::try_from(key.data.len()).expect("a key of at most 255 bytes");
        packet.extend_from_slice(&[key.key_type.into(), 0x00, len, 0x00]);
        packet.extend_from_slice(&key.data);
    }
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys as the wire has them - type byte, data - through the builder.
    fn packet(keys: &[(u8, &[u8])]) -> Vec<u8> {
        let keys: Vec<ProximityKey> = keys
            .iter()
            .map(|&(t, data)| ProximityKey {
                key_type: t.into(),
                data: data.to_vec(),
            })
            .collect();
        key_packet(&keys)
    }

    /// The layout of the example in `docs/aap-key-retrieval.md`: marker 0x31,
    /// count, then type, length and data per key, each header four bytes.
    #[test]
    fn builds_the_documented_layout() {
        let irk = vec![0xAA; 16];
        let enc = vec![0xBB; 16];
        let p = key_packet(&[
            ProximityKey {
                key_type: KeyType::Irk,
                data: irk.clone(),
            },
            ProximityKey {
                key_type: KeyType::EncKey,
                data: enc.clone(),
            },
        ]);
        assert_eq!(p.len(), 7 + 2 * (4 + 16));
        assert_eq!(p[..7], [0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x02]);
        assert_eq!(p[7..11], [0x01, 0x00, 0x10, 0x00]);
        assert_eq!(p[11..27], irk[..]);
        assert_eq!(p[27..31], [0x04, 0x00, 0x10, 0x00]);
        assert_eq!(p[31..], enc[..]);
    }

    /// A key packet prints its preamble alone, parsable or not; anything else
    /// prints whole.
    #[test]
    fn hex_leaves_the_keys_out() {
        let keys = packet(&[(0x01, &[0xAA; 16]), (0x04, &[0xBB; 16])]);
        assert_eq!(redacted_hex(&keys), "04000400310002 (keys redacted)");
        let mut truncated = keys.clone();
        truncated.truncate(12);
        assert_eq!(redacted_hex(&truncated), "04000400310002 (keys redacted)");
        assert_eq!(redacted_hex(&keys[..5]), "0400040031 (keys redacted)");

        assert_eq!(redacted_hex(&REQUEST), "0400040030000500");
    }

    #[test]
    fn wire_values_round_trip() {
        for v in 0..=u8::MAX {
            assert_eq!(u8::from(KeyType::from(v)), v);
        }
    }

    #[test]
    fn detects_key_packets() {
        assert!(is_key_packet(&packet(&[(0x04, &[0u8; 16])])));
        assert!(!is_key_packet(&[0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01]));
        assert!(!is_key_packet(&[0x04; 3]));
        // The marker alone is not enough; the header has to match too.
        assert!(!is_key_packet(&[
            0x05, 0x00, 0x04, 0x00, KEY_MARKER, 0x00, 0x01
        ]));
    }

    #[test]
    fn extracts_irk_and_enc_key() {
        let irk = [0xAAu8; 16];
        let enc = [0xBBu8; 16];
        let keys = parse_proximity_keys(&packet(&[(0x01, &irk), (0x04, &enc)])).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_type, KeyType::Irk);
        assert_eq!(keys[1].key_type, KeyType::EncKey);
        assert_eq!(find_encryption_key(&keys), Ok(enc));
        assert_eq!(find_irk(&keys), Some(&irk[..]));
    }

    #[test]
    fn reports_a_missing_enc_key() {
        let keys = parse_proximity_keys(&packet(&[(0x01, &[0xAAu8; 16])])).unwrap();
        assert_eq!(
            find_encryption_key(&keys),
            Err(KeyParseError::NoEncryptionKey)
        );
    }

    /// Stored, a key of any other length would overwrite a working one and then
    /// fail every decryption.
    #[test]
    fn rejects_an_enc_key_of_the_wrong_length() {
        for len in [0, 15, 17, 32] {
            let data = vec![0xBB; len];
            let keys = parse_proximity_keys(&packet(&[(0x04, data.as_slice())])).unwrap();
            assert_eq!(
                find_encryption_key(&keys),
                Err(KeyParseError::BadEncryptionKeyLen(len))
            );
        }
    }

    #[test]
    fn rejects_malformed_packets() {
        assert_eq!(
            parse_proximity_keys(&[0u8; 3]),
            Err(KeyParseError::TooShort(3))
        );
        assert_eq!(
            parse_proximity_keys(&[0x04, 0x00, 0x04, 0x00, 0x99, 0x00, 0x01]),
            Err(KeyParseError::NotKeyPacket)
        );
        assert_eq!(
            parse_proximity_keys(&[0x04, 0x00, 0x04, 0x00, KEY_MARKER, 0x00, 0x00]),
            Err(KeyParseError::NoKeys)
        );
        assert_eq!(
            parse_proximity_keys(&[0x04, 0x00, 0x04, 0x00, KEY_MARKER, 0x00, 99]),
            Err(KeyParseError::SuspiciousCount(99))
        );
    }

    #[test]
    fn detects_truncated_key_data() {
        let mut p = packet(&[(0x04, &[0xBBu8; 16])]);
        p.truncate(p.len() - 4);
        assert_eq!(
            parse_proximity_keys(&p),
            Err(KeyParseError::TruncatedData(1))
        );
    }
}
