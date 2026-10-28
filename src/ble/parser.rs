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

/// Which pod is primary - see the module docs for what that swaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProximityData {
    /// Apple's product ID - see [`decode_model_name`].
    pub product_id: u16,
    pub status: u8,
    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
    pub left_in_ear: bool,
    pub right_in_ear: bool,
    /// `None` unless the status byte puts a pod in the case - see
    /// [`parse_proximity_data`].
    pub lid_open: Option<bool>,
    /// Status bit 4, "one pod is in the case" in LibrePods' parser. A worn pod
    /// whose partner is in the case sets it: its own advertisement has no lid
    /// to report, while its partner's does. Clear whenever no pod is in the
    /// case.
    pub one_in_case: bool,
    pub color: u8,
    pub connection_state: u8,
    /// True when the right pod is primary.
    pub is_flipped: bool,
    pub raw_data: Vec<u8>,

    pub has_decrypted: bool,
}

impl ProximityData {
    /// The 16-byte encrypted slice, if the payload is long enough to carry one.
    pub fn encrypted_portion(&self) -> Option<&[u8]> {
        self.raw_data.get(ENCRYPTED_RANGE)
    }

    /// Merges decrypted battery data, replacing the cleartext's levels in 10%
    /// steps with exact ones.
    ///
    /// The app shows only advertisements it identifies - from an address a
    /// stored IRK resolves, decrypted by that pair's ENC_KEY - so it shows
    /// only these. The cleartext levels are what an unidentified pair
    /// advertises, decoded all the same: offering such a pair for pairing
    /// would read them (#131).
    ///
    /// Decrypted layout: byte 1 = first pod, byte 2 = second pod, byte 3 = case;
    /// in each, bit 7 is charging and bits 0-6 are the level.
    pub fn add_decrypted_data(&mut self, decrypted: &[u8]) -> Result<(), ParseError> {
        if decrypted.len() != ENCRYPTED_LEN {
            return Err(ParseError::BadDecryptedLen(decrypted.len()));
        }

        self.has_decrypted = true;

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
        // Little-endian, which makes it the product ID BlueZ's `Modalias` carries
        // (`p2027` for AirPods Pro 3). Read big-endian, as it once was, the same
        // model came out as 0x2720 - a second number for one device.
        product_id: u16::from_le_bytes([payload[1], payload[2]]),
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

    // Lid: byte 6, bit 3 clear means open - but only while a pod is in the case.
    // With them out, bit 3 reads 0 whatever the case is doing, which showed up as
    // a lid stuck on "Open" the whole time the pods were in someone's ears.
    // Status bit 6 says this pod is in the case, bit 2 that both are. Byte 6's
    // high nibble can't tell: AirPods Pro 3 sends 0x11 from the open case too.
    let in_case = status & 0x44 != 0;
    pd.lid_open = in_case.then(|| payload[6] & 0x08 == 0);
    pd.one_in_case = status & 0x10 != 0;
    // Byte 8, not 9 - 9 is the first byte of the encrypted portion.
    pd.connection_state = payload[8];

    Ok(pd)
}

/// 0x0-0x9 => 0-90% in 10% steps, 0xA-0xE => 100%, 0xF => unknown.
pub(crate) fn decode_battery(nibble: u8) -> Option<u8> {
    match nibble {
        0x0..=0x9 => Some(nibble * 10),
        0xA..=0xE => Some(100),
        _ => None,
    }
}

/// The inverse of [`decode_battery`]. Levels round down to their 10% step,
/// which is how captured advertisements compare with their decrypted levels:
/// 58% advertised as 50%, 14% as 10%.
#[cfg(any(test, feature = "harness"))]
fn encode_battery(level: Option<u8>) -> u8 {
    match level {
        None => 0xF,
        Some(level) if level >= 100 => 0xA,
        Some(level) => level / 10,
    }
}

/// The cleartext of a proximity pairing advertisement, field by field: what
/// [`Cleartext::to_bytes`] encodes and [`parse_proximity_data`] reads back.
#[cfg(any(test, feature = "harness"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleartext {
    /// Apple's product ID - see [`decode_model_name`].
    pub product_id: u16,
    pub primary: PodSide,
    /// Whether the pod sending the advertisement is in the case: status bit 6.
    /// With the primary pod, it decides which bit carries which ear.
    pub this_in_case: bool,
    /// Whether both pods are in the case: status bit 2.
    pub both_in_case: bool,
    /// Status bit 4, as [`ProximityData::one_in_case`] has it.
    pub one_in_case: bool,
    /// Percent, advertised in 10% steps.
    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
    pub left_in_ear: bool,
    pub right_in_ear: bool,
    /// `None` unless `this_in_case` or `both_in_case`, as [`ProximityData`]
    /// has it: the parser reads the lid only then.
    pub lid_open: Option<bool>,
    pub color: u8,
    pub connection_state: u8,
}

#[cfg(any(test, feature = "harness"))]
impl Cleartext {
    /// Payload bytes 0-8, everything before the encrypted portion, laid out the
    /// way [`parse_proximity_data`] reads them - flip and ear swap included.
    /// Status bits nobody has explained are left clear.
    pub fn to_bytes(&self) -> [u8; 9] {
        debug_assert_eq!(
            self.lid_open.is_some(),
            self.this_in_case || self.both_in_case,
            "the lid is advertised exactly while a pod is in the case"
        );
        let primary_left = self.primary == PodSide::Left;
        let is_flipped = !primary_left;
        let xor_factor = primary_left != self.this_in_case;

        let (bit3_in_ear, bit1_in_ear) = if xor_factor {
            (self.right_in_ear, self.left_in_ear)
        } else {
            (self.left_in_ear, self.right_in_ear)
        };
        let status = u8::from(self.this_in_case) << 6
            | u8::from(primary_left) << 5
            | u8::from(self.one_in_case) << 4
            | u8::from(bit3_in_ear) << 3
            | u8::from(self.both_in_case) << 2
            | u8::from(bit1_in_ear) << 1;

        // The primary pod takes the high battery nibble and charging bit 4.
        let (high, low) = if is_flipped {
            (self.right_battery, self.left_battery)
        } else {
            (self.left_battery, self.right_battery)
        };
        let (bit5_charging, bit4_charging) = if is_flipped {
            (self.left_charging, self.right_charging)
        } else {
            (self.right_charging, self.left_charging)
        };
        let charging = u8::from(self.case_charging) << 6
            | u8::from(bit5_charging) << 5
            | u8::from(bit4_charging) << 4
            | encode_battery(self.case_battery);

        // Bit 3 set for a closed lid; the rest as AirPods Pro 3 sends it: 0x11
        // out of the case, 0x31 from the open case and 0x39 from the closed one.
        // Nobody has explained bits 4-7 or the low bits.
        let lid = match self.lid_open {
            None => 0x11,
            Some(true) => 0x31,
            Some(false) => 0x39,
        };

        let [product_low, product_high] = self.product_id.to_le_bytes();
        [
            PAYLOAD_PREFIX,
            product_low,
            product_high,
            status,
            encode_battery(high) << 4 | encode_battery(low),
            charging,
            lid,
            self.color,
            self.connection_state,
        ]
    }
}

/// The value of the Apple (0x004C) entry in a device's `ManufacturerData`: the
/// message type, the payload length, then `cleartext` and `encrypted` as the
/// payload. This is what [`parse_proximity_data`] takes.
///
/// # Panics
///
/// With a payload longer than 255 bytes, which the length byte cannot hold.
#[cfg(any(test, feature = "harness"))]
pub fn proximity_message(cleartext: &[u8], encrypted: &[u8]) -> Vec<u8> {
    let length =
        u8::try_from(cleartext.len() + encrypted.len()).expect("a payload of at most 255 bytes");
    let mut data = vec![PROXIMITY_TYPE, length];
    data.extend_from_slice(cleartext);
    data.extend_from_slice(encrypted);
    data
}

pub(crate) fn decode_color(color: u8) -> String {
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
        // Said to be a color: shown beside the model name, a bare "Unknown"
        // would read as an unknown model.
        other => format!("Unknown color (0x{other:02X})"),
    }
}

pub(crate) fn decode_connection_state(state: u8) -> String {
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

/// The name of an AirPods model, by Apple's product ID; `None` for one not
/// listed here, so the caller decides what to show instead.
///
/// Only AirPods Pro 2 (USB-C) and AirPods Pro 3 are verified on hardware in this
/// project. The rest come from public Continuity tables, unverified here, and
/// are listed only where two of them agree: LibrePods
/// (librepods-org/librepods, `linux/ble/blemanager.cpp`, which writes the IDs
/// byte-swapped) and AirPodsDesktop (SpriteOvO/AirPodsDesktop,
/// `Source/Core/AppleCP.cpp`). furiousMAC/continuity's Wireshark dissector
/// lists the first two generations and the first AirPods Pro, byte-swapped too.
pub(crate) fn decode_model_name(product_id: u16) -> Option<&'static str> {
    let name = match product_id {
        0x2002 => "AirPods (1st gen)",
        0x200F => "AirPods (2nd gen)",
        0x2013 => "AirPods (3rd gen)",
        0x2019 => "AirPods 4",
        0x201B => "AirPods 4 (ANC)",
        0x200E => "AirPods Pro",
        0x2014 => "AirPods Pro (2nd gen)",
        0x2024 => "AirPods Pro (2nd gen, USB-C)", // verified on hardware
        0x2027 => "AirPods Pro 3",                // verified on hardware
        0x200A => "AirPods Max",
        0x201F => "AirPods Max (USB-C)",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::decrypt::{Plaintext, ZEROED_SUFFIX};

    /// Builds a minimal well-formed advertisement with a controllable status byte.
    /// The lid byte defaults to a captured lid-open value, which reads as open
    /// only when `status` puts a pod in the case.
    fn advert(status: u8, battery: u8, charging: u8) -> Vec<u8> {
        advert_with_lid(status, battery, charging, 0x31)
    }

    fn advert_with_lid(status: u8, battery: u8, charging: u8, lid: u8) -> Vec<u8> {
        let cleartext = [
            0x01,     // 0: prefix
            0x27,     // 1: product ID, low byte
            0x20,     // 2: product ID, high byte -> 0x2027, AirPods Pro 3
            status,   // 3: status
            battery,  // 4: battery nibbles
            charging, // 5: charging bits + case battery
            lid,      // 6: lid state
            0x00,     // 7: color
            0x05,     // 8: connection state -> Music
        ];
        // Only the first byte of the encrypted portion: the shortest payload the
        // parser takes.
        proximity_message(&cleartext, &[0x00])
    }

    /// A [`Cleartext`] of everything [`ProximityData`] says.
    fn cleartext_of(pd: &ProximityData) -> Cleartext {
        Cleartext {
            product_id: pd.product_id,
            primary: pd.primary_pod(),
            this_in_case: pd.status & 0x40 != 0,
            both_in_case: pd.status & 0x04 != 0,
            one_in_case: pd.one_in_case,
            left_battery: pd.left_battery,
            right_battery: pd.right_battery,
            case_battery: pd.case_battery,
            left_charging: pd.left_charging,
            right_charging: pd.right_charging,
            case_charging: pd.case_charging,
            left_in_ear: pd.left_in_ear,
            right_in_ear: pd.right_in_ear,
            lid_open: pd.lid_open,
            color: pd.color,
            connection_state: pd.connection_state,
        }
    }

    /// What the parser makes of `cleartext`, as a [`Cleartext`] again.
    fn reparsed(cleartext: &Cleartext) -> Cleartext {
        let data = proximity_message(&cleartext.to_bytes(), &[0; ENCRYPTED_LEN]);
        cleartext_of(&parse_proximity_data(&data).unwrap())
    }

    /// A decrypted payload carrying these batteries, each as level and charging:
    /// the primary pod, the other, the case.
    fn decrypted(primary: (u8, bool), secondary: (u8, bool), case: (u8, bool)) -> [u8; 16] {
        Plaintext {
            header: 0x10,
            primary_battery: Some(primary.0),
            primary_charging: primary.1,
            secondary_battery: Some(secondary.0),
            secondary_charging: secondary.1,
            case_battery: Some(case.0),
            case_charging: case.1,
            mac_suffix: ZEROED_SUFFIX,
            tail: [0; 4],
        }
        .to_bytes()
    }

    /// Every combination of primary pod, the in-case bits, ears, charging and
    /// lid comes back as it went in: the flip and the ear swap undo each other.
    /// The lid is there exactly while a pod is in the case.
    #[test]
    fn cleartext_round_trips_through_the_parser() {
        for primary in [PodSide::Left, PodSide::Right] {
            for (this_in_case, both_in_case) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let lids: &[Option<bool>] = if this_in_case || both_in_case {
                    &[Some(true), Some(false)]
                } else {
                    &[None]
                };
                for &lid_open in lids {
                    for bits in 0..64u8 {
                        let bit = |n: u8| bits & (1 << n) != 0;
                        let cleartext = Cleartext {
                            product_id: 0x2024,
                            primary,
                            this_in_case,
                            both_in_case,
                            one_in_case: bit(5),
                            left_battery: Some(80),
                            right_battery: Some(30),
                            case_battery: None,
                            left_charging: bit(0),
                            right_charging: bit(1),
                            case_charging: bit(2),
                            left_in_ear: bit(3),
                            right_in_ear: bit(4),
                            lid_open,
                            color: 0x01,
                            connection_state: 0x04,
                        };
                        assert_eq!(reparsed(&cleartext), cleartext);
                    }
                }
            }
        }
    }

    #[test]
    fn advertises_batteries_in_10_percent_steps() {
        for (level, advertised) in [
            (None, None),
            (Some(0), Some(0)),
            (Some(9), Some(0)),
            (Some(58), Some(50)),
            (Some(99), Some(90)),
            (Some(100), Some(100)),
        ] {
            let cleartext = Cleartext {
                left_battery: level,
                right_battery: level,
                case_battery: level,
                ..cleartext_of(&parse_proximity_data(&advert(0x20, 0x00, 0x00)).unwrap())
            };
            let back = reparsed(&cleartext);
            assert_eq!(back.left_battery, advertised, "{level:?}");
            assert_eq!(back.right_battery, advertised, "{level:?}");
            assert_eq!(back.case_battery, advertised, "{level:?}");
        }
    }

    /// The builder against hardware: the cleartext of captured advertisements -
    /// the ones in `examples/decrypt_probe.rs` - parsed and rebuilt comes back
    /// byte for byte, but for the bits nobody has explained: status bits 0 and
    /// 7, charging bit 7 and every bit of the lid byte but bit 3.
    #[test]
    fn rebuilds_captured_advertisements() {
        const KNOWN: [u8; 9] = [0xFF, 0xFF, 0xFF, 0x7E, 0xFF, 0x7F, 0x08, 0xFF, 0xFF];
        for captured in [
            [0x01, 0x27, 0x20, 0x05, 0x66, 0xf3, 0x51, 0x00, 0x00],
            [0x01, 0x27, 0x20, 0x14, 0x77, 0xf5, 0x59, 0x00, 0x00],
            [0x01, 0x27, 0x20, 0x0b, 0x78, 0x8f, 0x11, 0x00, 0x04],
            [0x01, 0x24, 0x20, 0x25, 0xaa, 0xf1, 0x51, 0x00, 0x00],
            [0x01, 0x24, 0x20, 0x34, 0xaa, 0xf1, 0x51, 0x00, 0x00],
        ] {
            let data = proximity_message(&captured, &[0; ENCRYPTED_LEN]);
            let rebuilt = cleartext_of(&parse_proximity_data(&data).unwrap()).to_bytes();
            for (i, mask) in KNOWN.iter().enumerate() {
                assert_eq!(
                    rebuilt[i] & mask,
                    captured[i] & mask,
                    "byte {i} of {captured:02x?}"
                );
            }
        }
    }

    #[test]
    fn parses_connection_state_from_byte_8() {
        let pd = parse_proximity_data(&advert(0x35, 0x76, 0xba)).unwrap();
        assert_eq!(pd.connection_state, 0x05);
        assert_eq!(decode_connection_state(pd.connection_state), "Music");
    }

    /// Captured lid bytes: open and closed, with and without an AAP link up,
    /// both pods in the case (status bit 2).
    #[test]
    fn lid_state_comes_from_byte_6() {
        for (lid, open) in [(0x52, true), (0x5A, false), (0x51, true), (0x59, false)] {
            let pd = parse_proximity_data(&advert_with_lid(0x35, 0x76, 0xba, lid)).unwrap();
            assert_eq!(pd.lid_open, Some(open), "lid byte {lid:#04x}");
        }
    }

    /// Status `0x0b` is a pod in someone's ear, with neither in-case bit set,
    /// so the lid byte - `0x11` here, its lid bit clear whether or not the case
    /// is shut - has nothing to report.
    #[test]
    fn lid_state_is_unknown_out_of_the_case() {
        let pd = parse_proximity_data(&advert_with_lid(0x0b, 0x76, 0x8f, 0x11)).unwrap();
        assert_eq!(pd.lid_open, None);
        assert!(!pd.one_in_case);
    }

    /// From the first #71 capture: with the left pod in the open case and the
    /// right worn, both advertise. The worn pod's `0x13` reads no lid, but its
    /// status bit 4 says a pod is in the case - the left pod, whose `0x73` reads
    /// the lid. With no pod in the case bit 4 is clear: both worn, both on the
    /// desk, one in hand.
    #[test]
    fn a_worn_pod_says_when_its_partner_is_in_the_case() {
        let parse = |cleartext: [u8; 9]| {
            parse_proximity_data(&proximity_message(&cleartext, &[0; ENCRYPTED_LEN])).unwrap()
        };
        let in_case = parse([0x01, 0x27, 0x20, 0x73, 0xaa, 0x94, 0x32, 0x00, 0x04]);
        assert_eq!((in_case.lid_open, in_case.one_in_case), (Some(true), true));
        let worn = parse([0x01, 0x27, 0x20, 0x13, 0xaa, 0xa4, 0x11, 0x00, 0x04]);
        assert_eq!((worn.lid_open, worn.one_in_case), (None, true));
        for status in [0x0b, 0x01, 0x29] {
            let pd = parse([0x01, 0x27, 0x20, status, 0xaa, 0x8f, 0x11, 0x00, 0x00]);
            assert_eq!(
                (pd.lid_open, pd.one_in_case),
                (None, false),
                "{status:#04x}"
            );
        }
    }

    /// The cleartext of AirPods Pro 3 advertisements from the second #71
    /// capture, which closed and opened the lid with the pods in the case, and
    /// the first, which had one pod in the case and one worn. Then the five
    /// September captures of `rebuilds_captured_advertisements`, Pro 3 and Pro 2.
    /// Only the status byte and byte 6 matter; the encrypted part is zeroed.
    #[test]
    fn decodes_the_lid_from_captured_advertisements() {
        for (state, cleartext, lid_open) in [
            (
                "lid open, right pod, first after opening",
                [0x01, 0x27, 0x20, 0x04, 0xaa, 0x93, 0x11, 0x00, 0x00],
                Some(true),
            ),
            (
                "lid open, right pod",
                [0x01, 0x27, 0x20, 0x15, 0xaa, 0xb3, 0x11, 0x00, 0x00],
                Some(true),
            ),
            (
                "lid open, left pod",
                [0x01, 0x27, 0x20, 0x75, 0xaa, 0xb3, 0x31, 0x00, 0x00],
                Some(true),
            ),
            (
                "lid closed, right pod",
                [0x01, 0x27, 0x20, 0x15, 0xaa, 0xb3, 0x39, 0x00, 0x00],
                Some(false),
            ),
            (
                "lid closed, left pod",
                [0x01, 0x27, 0x20, 0x75, 0xaa, 0xb3, 0x39, 0x00, 0x00],
                Some(false),
            ),
            (
                "pods out on the desk, right pod",
                [0x01, 0x27, 0x20, 0x01, 0xaa, 0x8f, 0x11, 0x00, 0x00],
                None,
            ),
            (
                "back in the case, lid open, right pod",
                [0x01, 0x27, 0x20, 0x55, 0xaa, 0xb3, 0x32, 0x00, 0x00],
                Some(true),
            ),
            (
                "back in the case, lid open, left pod",
                [0x01, 0x27, 0x20, 0x35, 0xaa, 0xb3, 0x32, 0x00, 0x00],
                Some(true),
            ),
            (
                "in the case, connected",
                [0x01, 0x27, 0x20, 0x55, 0xaa, 0xb3, 0x32, 0x00, 0x04],
                Some(true),
            ),
            (
                "being put back, right pod already in",
                [0x01, 0x27, 0x20, 0x51, 0xaa, 0x93, 0x12, 0x00, 0x00],
                Some(true),
            ),
            (
                "being put back, left pod in hand",
                [0x01, 0x27, 0x20, 0x29, 0xaa, 0x8f, 0x11, 0x00, 0x00],
                None,
            ),
            (
                "left pod in the case, right worn",
                [0x01, 0x27, 0x20, 0x73, 0xaa, 0x94, 0x32, 0x00, 0x04],
                Some(true),
            ),
            (
                "right pod worn, left in the case",
                [0x01, 0x27, 0x20, 0x13, 0xaa, 0xa4, 0x11, 0x00, 0x04],
                None,
            ),
            (
                "both worn",
                [0x01, 0x27, 0x20, 0x0b, 0xaa, 0x8f, 0x11, 0x00, 0x04],
                None,
            ),
            (
                "September, Pro 3, open",
                [0x01, 0x27, 0x20, 0x05, 0x66, 0xf3, 0x51, 0x00, 0x00],
                Some(true),
            ),
            (
                "September, Pro 3, closed",
                [0x01, 0x27, 0x20, 0x14, 0x77, 0xf5, 0x59, 0x00, 0x00],
                Some(false),
            ),
            (
                "September, Pro 3, worn",
                [0x01, 0x27, 0x20, 0x0b, 0x78, 0x8f, 0x11, 0x00, 0x04],
                None,
            ),
            (
                "September, Pro 2, open",
                [0x01, 0x24, 0x20, 0x25, 0xaa, 0xf1, 0x51, 0x00, 0x00],
                Some(true),
            ),
            (
                "September, Pro 2, open",
                [0x01, 0x24, 0x20, 0x34, 0xaa, 0xf1, 0x51, 0x00, 0x00],
                Some(true),
            ),
        ] {
            let data = proximity_message(&cleartext, &[0; ENCRYPTED_LEN]);
            let pd = parse_proximity_data(&data).unwrap();
            assert_eq!(pd.lid_open, lid_open, "{state}: {cleartext:02x?}");
        }
    }

    /// Byte 6's high nibble is no gate: 0x5 appears only in the September
    /// captures, all with the case charging, so it may follow the case's
    /// charging rather than the pods. Out of the case by the status byte, a 0x5
    /// lid byte says nothing.
    #[test]
    fn a_0x5_lid_byte_alone_reports_no_lid() {
        for lid in [0x51, 0x59] {
            let pd = parse_proximity_data(&advert_with_lid(0x0b, 0x76, 0xcf, lid)).unwrap();
            assert_eq!(pd.lid_open, None, "lid byte {lid:#04x}");
        }
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
        // bit 5 set -> primary is left -> not flipped; bit 2 set -> both pods
        // in the case, so the lid is read.
        let pd = parse_proximity_data(&advert(0b0010_0100, 0x87, 0x00)).unwrap();
        assert!(!pd.is_flipped);
        assert_eq!(pd.primary_pod(), PodSide::Left);
        assert_eq!(pd.left_battery, Some(80));
        assert_eq!(pd.right_battery, Some(70));
        assert_eq!(pd.product_id, 0x2027);
        assert_eq!(decode_model_name(pd.product_id), Some("AirPods Pro 3"));
        assert_eq!(pd.lid_open, Some(true));
    }

    /// The two models verified on hardware, from the cleartext of captured
    /// advertisements, encrypted portion zeroed: the product ID is the one
    /// their `Modalias` carries, `p2027` and `p2024` - not the byte-swapped
    /// 0x2720 and 0x2420 a big-endian read gave.
    #[test]
    fn reads_the_product_id_little_endian() {
        for (header, product_id, name) in [
            (
                [0x01, 0x27, 0x20, 0x05, 0x66, 0xf3, 0x51, 0x00, 0x00],
                0x2027,
                "AirPods Pro 3",
            ),
            (
                [0x01, 0x24, 0x20, 0x25, 0xaa, 0xf1, 0x51, 0x00, 0x00],
                0x2024,
                "AirPods Pro (2nd gen, USB-C)",
            ),
        ] {
            let data = proximity_message(&header, &[0; ENCRYPTED_LEN]);

            let pd = parse_proximity_data(&data).unwrap();
            assert_eq!(pd.product_id, product_id);
            assert_eq!(decode_model_name(pd.product_id), Some(name));
        }
    }

    /// Regression (#45): 0x2002 was labeled the 2nd generation.
    #[test]
    fn names_the_first_two_generations_apart() {
        assert_eq!(decode_model_name(0x2002), Some("AirPods (1st gen)"));
        assert_eq!(decode_model_name(0x200F), Some("AirPods (2nd gen)"));
    }

    /// No name rather than a made-up one, so the caller decides the fallback.
    /// The byte-swapped Pro 3 ID is not a product.
    #[test]
    fn an_unknown_product_id_has_no_name() {
        assert_eq!(decode_model_name(0x2720), None);
        assert_eq!(decode_model_name(0x0000), None);
    }

    /// A code the table lacks says it is a color: shown beside the model
    /// name, a bare "Unknown" would read as an unknown model.
    #[test]
    fn names_colors_and_says_what_an_unknown_code_is() {
        assert_eq!(decode_color(0x00), "White");
        assert_eq!(decode_color(0x42), "Unknown color (0x42)");
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
        // First pod charging at 63%, second pod 41%, case 77%.
        pd.add_decrypted_data(&decrypted((63, true), (41, false), (77, false)))
            .unwrap();

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
        pd.add_decrypted_data(&decrypted((63, false), (41, false), (77, false)))
            .unwrap();
        // Flipped: byte1 is the right pod.
        assert_eq!(pd.right_battery, Some(63));
        assert_eq!(pd.left_battery, Some(41));
    }

    #[test]
    fn rejects_invalid_decrypted_levels() {
        let mut pd = parse_proximity_data(&advert(0b0010_0000, 0x87, 0x00)).unwrap();
        // 127 > 100 -> unknown
        pd.add_decrypted_data(&decrypted((127, false), (0, false), (0, false)))
            .unwrap();
        assert_eq!(pd.left_battery, None);
    }

    /// The decrypted batteries as the builder writes them, read back on both
    /// sides of the flip.
    #[test]
    fn decrypted_batteries_round_trip() {
        for (status, primary_is_left) in [(0b0010_0000, true), (0b0000_0000, false)] {
            let mut pd = parse_proximity_data(&advert(status, 0x87, 0x00)).unwrap();
            let plain = Plaintext {
                header: 0x00,
                primary_battery: Some(12),
                primary_charging: true,
                secondary_battery: None,
                secondary_charging: false,
                case_battery: Some(100),
                case_charging: true,
                mac_suffix: ZEROED_SUFFIX,
                tail: [0; 4],
            };
            pd.add_decrypted_data(&plain.to_bytes()).unwrap();

            let (primary, secondary) = if primary_is_left {
                (
                    (pd.left_battery, pd.left_charging),
                    (pd.right_battery, pd.right_charging),
                )
            } else {
                (
                    (pd.right_battery, pd.right_charging),
                    (pd.left_battery, pd.left_charging),
                )
            };
            assert_eq!(primary, (Some(12), true));
            assert_eq!(secondary, (None, false));
            assert_eq!((pd.case_battery, pd.case_charging), (Some(100), true));
        }
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
