//! AAP proximity key packet parsing.
//!
//! Packet layout:
//!   0-1: header (04 00)   2-3: command (04 00)
//!   4:   key marker 0x31  5: unknown   6: key count
//! Then per key: [type] [unknown] [len] [unknown] [data...]

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
    #[error("not a key packet (byte[4]=0x{0:02X}, expected 0x31)")]
    NotKeyPacket(u8),
    #[error("no keys in packet (key count = 0)")]
    NoKeys,
    #[error("suspicious key count: {0} (expected 1-10)")]
    SuspiciousCount(usize),
    #[error("packet too short for key {0} header")]
    TruncatedHeader(usize),
    #[error("packet too short for key {0} data")]
    TruncatedData(usize),
}

const KEY_MARKER: u8 = 0x31;
const MAX_KEYS: usize = 10;

pub fn is_key_packet(packet: &[u8]) -> bool {
    packet.len() >= 7 && packet[4] == KEY_MARKER
}

pub fn parse_proximity_keys(packet: &[u8]) -> Result<Vec<ProximityKey>, KeyParseError> {
    if packet.len() < 7 {
        return Err(KeyParseError::TooShort(packet.len()));
    }
    if packet[4] != KEY_MARKER {
        return Err(KeyParseError::NotKeyPacket(packet[4]));
    }

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
pub fn find_encryption_key(keys: &[ProximityKey]) -> Option<&[u8]> {
    keys.iter()
        .find(|k| k.key_type == KeyType::EncKey)
        .map(|k| k.data.as_slice())
}

pub fn find_irk(keys: &[ProximityKey]) -> Option<&[u8]> {
    keys.iter()
        .find(|k| k.key_type == KeyType::Irk)
        .map(|k| k.data.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(keys: &[(u8, &[u8])]) -> Vec<u8> {
        let mut p = vec![0x04, 0x00, 0x04, 0x00, KEY_MARKER, 0x00, keys.len() as u8];
        for (t, data) in keys {
            p.extend_from_slice(&[*t, 0x00, data.len() as u8, 0x00]);
            p.extend_from_slice(data);
        }
        p
    }

    #[test]
    fn detects_key_packets() {
        assert!(is_key_packet(&packet(&[(0x04, &[0u8; 16])])));
        assert!(!is_key_packet(&[0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01]));
        assert!(!is_key_packet(&[0x04; 3]));
    }

    #[test]
    fn extracts_irk_and_enc_key() {
        let irk = [0xAAu8; 16];
        let enc = [0xBBu8; 16];
        let keys = parse_proximity_keys(&packet(&[(0x01, &irk), (0x04, &enc)])).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_type, KeyType::Irk);
        assert_eq!(keys[1].key_type, KeyType::EncKey);
        assert_eq!(find_encryption_key(&keys), Some(&enc[..]));
        assert_eq!(find_irk(&keys), Some(&irk[..]));
    }

    #[test]
    fn returns_none_when_enc_key_absent() {
        let keys = parse_proximity_keys(&packet(&[(0x01, &[0xAAu8; 16])])).unwrap();
        assert_eq!(find_encryption_key(&keys), None);
    }

    #[test]
    fn rejects_malformed_packets() {
        assert_eq!(
            parse_proximity_keys(&[0u8; 3]),
            Err(KeyParseError::TooShort(3))
        );
        assert_eq!(
            parse_proximity_keys(&[0x04, 0x00, 0x04, 0x00, 0x99, 0x00, 0x01]),
            Err(KeyParseError::NotKeyPacket(0x99))
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
