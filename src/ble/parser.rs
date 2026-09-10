//! Apple Continuity proximity pairing parser.
//!
//! Includes the flip handling: the AirPods
//! advertise which pod is "primary", and when the right pod is primary the
//! battery nibbles, charging bits and ear-detection bits are all swapped so that
//! "left" and "right" always refer to the physical pods.

use super::decrypt::ENCRYPTED_LEN;

const PROXIMITY_TYPE: u8 = 0x07;
const PAYLOAD_PREFIX: u8 = 0x01;
const MIN_PAYLOAD: usize = 10;

/// Byte range of the encrypted portion within the payload.
pub const ENCRYPTED_RANGE: std::ops::Range<usize> = 9..9 + ENCRYPTED_LEN;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("data too short")]
    TooShort,
    #[error("not a proximity pairing message")]
    NotProximity,
    #[error("incomplete data")]
    Incomplete,
    #[error("payload too short")]
    PayloadTooShort,
    #[error("invalid prefix")]
    InvalidPrefix,
    #[error("decrypted data must be 16 bytes, got {0}")]
    BadDecryptedLen(usize),
}

/// Which pod is primary. Mirrors podstate.PodSide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PodSide {
    #[default]
    Unknown,
    Left,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProximityData {
    pub device_model: u16,
    pub status: u8,
    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
    pub left_in_ear: bool,
    pub right_in_ear: bool,
    /// `None` while the earbuds are out of the case - see [`parse_proximity_data`].
    pub lid_open: Option<bool>,
    pub color: u8,
    pub connection_state: u8,
    /// True when the right pod is primary.
    pub is_flipped: bool,
    pub raw_data: Vec<u8>,

    pub has_decrypted: bool,
    pub raw_decrypted: Option<[u8; ENCRYPTED_LEN]>,
}

impl ProximityData {
    /// The 16-byte encrypted slice, if the payload is long enough to carry one.
    pub fn encrypted_portion(&self) -> Option<&[u8]> {
        self.raw_data.get(ENCRYPTED_RANGE)
    }

    /// Merges decrypted battery data, replacing the coarse 10%-granularity values
    /// from the cleartext advertisement with exact ones.
    ///
    /// Decrypted layout: byte 1 = first pod, byte 2 = second pod, byte 3 = case;
    /// in each, bit 7 is charging and bits 0-6 are the level.
    pub fn add_decrypted_data(&mut self, decrypted: &[u8]) -> Result<(), ParseError> {
        if decrypted.len() != ENCRYPTED_LEN {
            return Err(ParseError::BadDecryptedLen(decrypted.len()));
        }

        self.has_decrypted = true;
        let mut buf = [0u8; ENCRYPTED_LEN];
        buf.copy_from_slice(decrypted);
        self.raw_decrypted = Some(buf);

        // Levels above 100 are invalid and mean "unknown".
        let split = |b: u8| -> (Option<u8>, bool) {
            let charging = (b & 0x80) != 0;
            let level = b & 0x7F;
            (if level <= 100 { Some(level) } else { None }, charging)
        };

        let (bat1, chg1) = split(decrypted[1]);
        let (bat2, chg2) = split(decrypted[2]);
        let (case_bat, case_chg) = split(decrypted[3]);

        if self.is_flipped {
            self.left_battery = bat2;
            self.right_battery = bat1;
            self.left_charging = chg2;
            self.right_charging = chg1;
        } else {
            self.left_battery = bat1;
            self.right_battery = bat2;
            self.left_charging = chg1;
            self.right_charging = chg2;
        }

        // Case is independent of flip.
        self.case_battery = case_bat;
        if case_bat.is_some() {
            self.case_charging = case_chg;
        }

        Ok(())
    }

    pub fn primary_pod(&self) -> PodSide {
        if self.is_flipped {
            PodSide::Right
        } else {
            PodSide::Left
        }
    }
}

/// Parses an Apple Continuity proximity pairing advertisement.
pub fn parse_proximity_data(data: &[u8]) -> Result<ProximityData, ParseError> {
    if data.len() < 2 {
        return Err(ParseError::TooShort);
    }
    if data[0] != PROXIMITY_TYPE {
        return Err(ParseError::NotProximity);
    }

    let length = data[1] as usize;
    if data.len() < 2 + length {
        return Err(ParseError::Incomplete);
    }
    let payload = &data[2..2 + length];

    if payload.len() < MIN_PAYLOAD {
        return Err(ParseError::PayloadTooShort);
    }
    if payload[0] != PAYLOAD_PREFIX {
        return Err(ParseError::InvalidPrefix);
    }

    let status = payload[3];
    let primary_left = ((status >> 5) & 0x01) == 1;
    let this_in_case = ((status >> 6) & 0x01) == 1;
    let is_flipped = !primary_left;
    // XOR decides whether ear-detection bits are swapped.
    let xor_factor = primary_left != this_in_case;

    let mut pd = ProximityData {
        device_model: u16::from(payload[1]) << 8 | u16::from(payload[2]),
        status,
        is_flipped,
        color: payload[7],
        raw_data: payload.to_vec(),
        ..Default::default()
    };

    // Battery nibbles from byte 4, swapped when flipped.
    let battery_byte = payload[4];
    let (left_nibble, right_nibble) = if is_flipped {
        (battery_byte & 0x0F, (battery_byte >> 4) & 0x0F)
    } else {
        ((battery_byte >> 4) & 0x0F, battery_byte & 0x0F)
    };
    pd.left_battery = decode_battery(left_nibble);
    pd.right_battery = decode_battery(right_nibble);

    // Byte 5 carries both the case battery nibble and the charging bits.
    let charging_byte = payload[5];
    pd.case_battery = decode_battery(charging_byte & 0x0F);
    pd.case_charging = ((charging_byte >> 6) & 0x01) != 0;
    pd.right_charging = ((charging_byte >> 5) & 0x01) != 0;
    pd.left_charging = ((charging_byte >> 4) & 0x01) != 0;
    if is_flipped {
        std::mem::swap(&mut pd.left_charging, &mut pd.right_charging);
    }

    pd.left_in_ear = (status & 0x08) != 0;
    pd.right_in_ear = (status & 0x02) != 0;
    if xor_factor {
        std::mem::swap(&mut pd.left_in_ear, &mut pd.right_in_ear);
    }

    // Lid: byte 6, bit 3 clear means open - but the byte describes the case, and
    // only carries a lid state worth reading while the earbuds are inside it. With
    // them out, bit 3 reads 0 whatever the case is doing, which showed up as a lid
    // stuck on "Open" the whole time the pods were in someone's ears. Bits 4-7
    // separate the two: 0x5 in the case, 0x1 out of it.
    let pods_in_case = (payload[6] >> 4) == 0x05;
    pd.lid_open = pods_in_case.then(|| ((payload[6] >> 3) & 0x01) == 0);
    // Byte 8, not 9 - 9 is the first byte of the encrypted portion.
    pd.connection_state = payload[8];

    Ok(pd)
}

/// 0x0-0x9 => 0-90% in 10% steps, 0xA-0xE => 100%, 0xF => unknown.
pub fn decode_battery(nibble: u8) -> Option<u8> {
    match nibble {
        0x0..=0x9 => Some(nibble * 10),
        0xA..=0xE => Some(100),
        _ => None,
    }
}

pub fn decode_color(color: u8) -> String {
    match color {
        0x00 => "White".into(),
        0x01 => "Black".into(),
        0x02 => "Red".into(),
        0x03 => "Blue".into(),
        0x04 => "Pink".into(),
        0x05 => "Gray".into(),
        0x06 => "Silver".into(),
        0x07 => "Gold".into(),
        0x08 => "Rose Gold".into(),
        0x09 => "Space Gray".into(),
        0x0A => "Dark Blue".into(),
        0x0B => "Light Blue".into(),
        0x0C => "Yellow".into(),
        other => format!("Unknown (0x{other:02X})"),
    }
}

pub fn decode_connection_state(state: u8) -> String {
    match state {
        0x00 => "Disconnected".into(),
        0x04 => "Idle".into(),
        0x05 => "Music".into(),
        0x06 => "Call".into(),
        0x07 => "Ringing".into(),
        0x09 => "Hanging Up".into(),
        0xFF => "Unknown".into(),
        other => format!("Unknown (0x{other:02X})"),
    }
}

pub fn decode_model_name(device_model: u16) -> String {
    match device_model {
        0x0220 => "AirPods (2nd gen)".into(),
        0x0e20 => "AirPods Pro".into(),
        0x2420 => "AirPods Pro (2nd gen)".into(),
        0x2720 => "AirPods Pro 3".into(),
        other => format!("Unknown (0x{other:04X})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal well-formed advertisement with a controllable status byte.
    /// The lid byte defaults to a captured in-case, lid-open value.
    fn advert(status: u8, battery: u8, charging: u8) -> Vec<u8> {
        advert_with_lid(status, battery, charging, 0x51)
    }

    fn advert_with_lid(status: u8, battery: u8, charging: u8, lid: u8) -> Vec<u8> {
        let payload = vec![
            0x01,     // 0: prefix
            0x27,     // 1: model hi
            0x20,     // 2: model lo -> AirPods Pro 3
            status,   // 3: status
            battery,  // 4: battery nibbles
            charging, // 5: charging bits + case battery
            lid,      // 6: in-case indicator + lid state
            0x00,     // 7: colour
            0x05,     // 8: connection state -> Music
            0x00,     // 9: first byte of the encrypted portion
        ];
        let mut v = vec![PROXIMITY_TYPE, payload.len() as u8];
        v.extend_from_slice(&payload);
        v
    }

    #[test]
    fn parses_connection_state_from_byte_8() {
        let pd = parse_proximity_data(&advert(0x35, 0x76, 0xba)).unwrap();
        assert_eq!(pd.connection_state, 0x05);
        assert_eq!(decode_connection_state(pd.connection_state), "Music");
    }

    /// Captured lid bytes: open and closed, with and without an AAP link up.
    #[test]
    fn lid_state_comes_from_byte_6() {
        for (lid, open) in [(0x52, true), (0x5A, false), (0x51, true), (0x59, false)] {
            let pd = parse_proximity_data(&advert_with_lid(0x35, 0x76, 0xba, lid)).unwrap();
            assert_eq!(pd.lid_open, Some(open), "lid byte {lid:#04x}");
        }
    }

    /// `0x11` is what the earbuds report from someone's ears, and its lid bit is
    /// clear whether or not the case is shut - so there is nothing to report.
    #[test]
    fn lid_state_is_unknown_out_of_the_case() {
        let pd = parse_proximity_data(&advert_with_lid(0x0b, 0x76, 0x8f, 0x11)).unwrap();
        assert_eq!(pd.lid_open, None);
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse_proximity_data(&[]), Err(ParseError::TooShort));
        assert_eq!(
            parse_proximity_data(&[0x01, 0x00]),
            Err(ParseError::NotProximity)
        );
        assert_eq!(
            parse_proximity_data(&[0x07, 0x40]),
            Err(ParseError::Incomplete)
        );
        assert_eq!(
            parse_proximity_data(&[0x07, 0x02, 0x00, 0x00]),
            Err(ParseError::PayloadTooShort)
        );
    }

    #[test]
    fn parses_unflipped_layout() {
        // bit 5 set -> primary is left -> not flipped.
        let pd = parse_proximity_data(&advert(0b0010_0000, 0x87, 0x00)).unwrap();
        assert!(!pd.is_flipped);
        assert_eq!(pd.primary_pod(), PodSide::Left);
        assert_eq!(pd.left_battery, Some(80));
        assert_eq!(pd.right_battery, Some(70));
        assert_eq!(pd.device_model, 0x2720);
        assert_eq!(decode_model_name(pd.device_model), "AirPods Pro 3");
        assert_eq!(pd.lid_open, Some(true));
    }

    #[test]
    fn swaps_batteries_when_flipped() {
        // bit 5 clear -> primary is right -> flipped.
        let pd = parse_proximity_data(&advert(0b0000_0000, 0x87, 0x00)).unwrap();
        assert!(pd.is_flipped);
        assert_eq!(pd.primary_pod(), PodSide::Right);
        assert_eq!(pd.left_battery, Some(70));
        assert_eq!(pd.right_battery, Some(80));
    }

    #[test]
    fn decodes_battery_nibbles() {
        assert_eq!(decode_battery(0x0), Some(0));
        assert_eq!(decode_battery(0x5), Some(50));
        assert_eq!(decode_battery(0x9), Some(90));
        assert_eq!(decode_battery(0xA), Some(100));
        assert_eq!(decode_battery(0xE), Some(100));
        assert_eq!(decode_battery(0xF), None);
    }

    #[test]
    fn decrypted_data_overrides_coarse_levels() {
        let mut pd = parse_proximity_data(&advert(0b0010_0000, 0x87, 0x00)).unwrap();
        let mut dec = [0u8; 16];
        dec[4] = 0x2D;
        dec[1] = 0x80 | 63; // first pod: charging, 63%
        dec[2] = 41; // second pod: 41%
        dec[3] = 77; // case: 77%
        pd.add_decrypted_data(&dec).unwrap();

        assert!(pd.has_decrypted);
        assert_eq!(pd.left_battery, Some(63));
        assert!(pd.left_charging);
        assert_eq!(pd.right_battery, Some(41));
        assert!(!pd.right_charging);
        assert_eq!(pd.case_battery, Some(77));
    }

    #[test]
    fn decrypted_data_respects_flip() {
        let mut pd = parse_proximity_data(&advert(0b0000_0000, 0x87, 0x00)).unwrap();
        let mut dec = [0u8; 16];
        dec[4] = 0x2D;
        dec[1] = 63;
        dec[2] = 41;
        dec[3] = 77;
        pd.add_decrypted_data(&dec).unwrap();
        // Flipped: byte1 is the right pod.
        assert_eq!(pd.right_battery, Some(63));
        assert_eq!(pd.left_battery, Some(41));
    }

    #[test]
    fn rejects_invalid_decrypted_levels() {
        let mut pd = parse_proximity_data(&advert(0b0010_0000, 0x87, 0x00)).unwrap();
        let mut dec = [0u8; 16];
        dec[4] = 0x2D;
        dec[1] = 127; // > 100 -> unknown
        pd.add_decrypted_data(&dec).unwrap();
        assert_eq!(pd.left_battery, None);
    }

    #[test]
    fn rejects_wrong_decrypted_length() {
        let mut pd = parse_proximity_data(&advert(0b0010_0000, 0x87, 0x00)).unwrap();
        assert_eq!(
            pd.add_decrypted_data(&[0u8; 8]),
            Err(ParseError::BadDecryptedLen(8))
        );
    }
}
