//! The device information packet (opcode 0x1D) the pods send at the start of
//! every connection, unasked: the pods' name and their firmware version are
//! read out of it, and nothing else.
//!
//! Format: `04 00 04 00 1D 00`, `02`, a little-endian u16 length of what
//! follows (`f8 00`, 248, in a 257-byte packet), `04 00`, then NUL-terminated
//! strings - binary bytes after them, not read. The strings' names are
//! LibrePods' (`docs/device-info.md`) and the apple-wireshark dissector's
//! (`plugins/aacp/aacp_enums.lua`): field 0 is the name, field 10 the
//! dissector's Marketing Version, and among the rest are the model number
//! and three serial numbers. Only fields 0 and 10 leave this module; the
//! serial numbers are never read, and the packet is never logged - see
//! [`crate::aap::keys::redacted_hex`]. See `docs/aap-device-info.md`.
//!
//! Where each comes from: the name's place was seen on a Pro 3 and a Pro 2
//! (USB-C), and a rename's name came back in it (#160). Field 10 held
//! `9442752` on both on 2026-11-02, when the iPhone showed `9A348` for both;
//! how [`FirmwareVersion`] reads it is ours, read off that one value, and
//! waits for the next firmware update to be confirmed (#161).

use crate::aap::message;

/// The device information opcode.
pub const OPCODE: u8 = 0x1D;

/// What the packet carries ahead of its strings: the header, the opcode,
/// `00`, `02`, the u16 length and `04 00`. The `02` and the `04 00` are
/// checked, as every 0x1D seen had them; the length is not.
pub const PREAMBLE_LEN: usize = 11;

/// Byte 6 of every device information packet seen.
const KIND: u8 = 0x02;
/// Bytes 9-10 of every device information packet seen, just ahead of the
/// name: counted in the length, unlike what comes before them.
const STRINGS_START: [u8; 2] = [0x04, 0x00];

/// Which string holds the Marketing Version, counting the name as 0: the
/// dissector's name for it. LibrePods calls it "Version (?)".
pub const VERSION_FIELD: usize = 10;

/// What a device information packet says of the pods, each field read on its
/// own: a name that is not UTF-8 leaves the version, and a version that does
/// not read leaves the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The pods' own name, the first string.
    pub name: Result<String, FieldError>,
    /// The firmware version, from the Marketing Version, field 10.
    pub version: Result<FirmwareVersion, FieldError>,
}

/// The two fields as a probe prints them: `name "Pods", firmware 9A348
/// (9442752)`, and why a field was not read in its place. Nothing else of
/// the packet is in it.
impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Ok(name) => write!(f, "name {name:?}")?,
            Err(e) => write!(f, "name not read: {e}")?,
        }
        match self.version {
            Ok(version) => write!(f, ", firmware {version} ({})", version.raw()),
            Err(e) => write!(f, ", firmware not read: {e}"),
        }
    }
}

/// Why a field of a device information packet was not read. Never carries
/// the bytes: a field out of place could be a serial number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FieldError {
    #[error("not in the packet")]
    Missing,
    #[error("not UTF-8")]
    NotUtf8,
    #[error("not a decimal number")]
    NotANumber,
}

/// Why a packet's device information cannot be read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InfoParseError {
    #[error("not a device information packet")]
    NotInfoPacket,
}

/// The firmware version as the iPhone shows it, `9A348`: the Marketing
/// Version (field 10), a decimal number, read as a packed value.
///
/// `9442752` is `0x9015C0`, read as:
///
/// | bits | | `0x9015C0` |
/// |---|---|---|
/// | 20 and up | major version | `9` |
/// | 16-19 | letter, `A` = 0 | `A` |
/// | 4-15 | build, decimal | `0x15C` = `348` |
/// | 0-3 | suffix letter, `a` = 1, none for 0 | none |
///
/// Ours, read off one value seen on two pairs on 2026-11-02, not the
/// dissector's (it prints the field as a string) and not confirmed by a
/// second firmware (#161). The low nibble is a guess on top: Apple's beta
/// builds end in a lowercase letter, and LibrePods quotes `8454371` for the
/// field, `0x8100E3`, which reads `8B14c` so - no release known, so maybe a
/// beta. Bits 20 and up are taken as the major's - a guess as well - so
/// that a 10 reads as `10` rather than wrapping. The value as sent is kept,
/// [`FirmwareVersion::raw`], and logged beside this reading, so a wrong one
/// shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FirmwareVersion(u32);

impl FirmwareVersion {
    /// The version field 10 gives as `raw`.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Field 10's value, as the pods sent it.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let major = self.0 >> 20;
        // Masked to a nibble each, so both fit a u8.
        let letter = char::from(b'A' + ((self.0 >> 16) & 0xF) as u8);
        let build = (self.0 >> 4) & 0xFFF;
        write!(f, "{major}{letter}{build}")?;
        match (self.0 & 0xF) as u8 {
            0 => Ok(()),
            suffix => write!(f, "{}", char::from(b'a' + suffix - 1)),
        }
    }
}

/// True for a device information packet: the AAP header, then 0x1D, `02`
/// at byte 6 and `04 00` at bytes 9-10, and longer than its preamble. Any
/// other is no packet this module knows - a 0x1D shaped otherwise is not
/// read, as a 0x09 report is not read for a mode unless byte 6 names the
/// setting.
pub fn is_info_packet(packet: &[u8]) -> bool {
    packet.len() > PREAMBLE_LEN
        && message::opcode(packet) == Some(OPCODE)
        && packet[6] == KIND
        && packet[9..PREAMBLE_LEN] == STRINGS_START
}

/// The packet's NUL-terminated strings, in order, without their NULs. Bytes
/// after the last NUL are no string.
fn fields(packet: &[u8]) -> impl Iterator<Item = &[u8]> {
    packet[PREAMBLE_LEN..]
        .split_inclusive(|&b| b == 0)
        .filter_map(|field| field.strip_suffix(&[0]))
}

/// The name's bytes in a device information packet, as sent, UTF-8 or not:
/// `None` for any other packet, or one whose name is unterminated.
pub fn info_name_bytes(packet: &[u8]) -> Option<&[u8]> {
    if !is_info_packet(packet) {
        return None;
    }
    fields(packet).next()
}

/// Reads the name and the firmware version out of a device information
/// packet.
pub fn parse_device_info(packet: &[u8]) -> Result<DeviceInfo, InfoParseError> {
    if !is_info_packet(packet) {
        return Err(InfoParseError::NotInfoPacket);
    }
    Ok(read(packet))
}

/// [`parse_device_info`] past its header check, for a packet
/// [`is_info_packet`] has accepted.
pub(super) fn read(packet: &[u8]) -> DeviceInfo {
    let mut fields = fields(packet);
    let name = fields
        .next()
        .ok_or(FieldError::Missing)
        .and_then(|name| String::from_utf8(name.to_vec()).map_err(|_| FieldError::NotUtf8));
    let version = fields
        .nth(VERSION_FIELD - 1)
        .ok_or(FieldError::Missing)
        .and_then(read_version);
    DeviceInfo { name, version }
}

/// Field 10 as a number: decimal digits only, as every one seen was, and
/// within a u32.
fn read_version(field: &[u8]) -> Result<FirmwareVersion, FieldError> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
        return Err(FieldError::NotANumber);
    }
    std::str::from_utf8(field)
        .ok()
        .and_then(|digits| digits.parse().ok())
        .map(FirmwareVersion)
        .ok_or(FieldError::NotANumber)
}

/// A device information packet as the pods send it, laid out as above:
/// `name`, then `rest`, each NUL-terminated. The fake pods' and the tests':
/// `rest` stands in for fields 1 and on - the model, the serial numbers and
/// the rest made up, field 10 a version where it reaches that far. The name
/// is bytes, as the pods hold it: one cut inside a character is not UTF-8.
///
/// # Panics
///
/// With strings longer than the u16 length can count.
#[cfg(any(test, feature = "harness"))]
pub fn info_packet(name: impl AsRef<[u8]>, rest: &[&str]) -> Vec<u8> {
    let mut strings = STRINGS_START.to_vec();
    for text in std::iter::once(name.as_ref()).chain(rest.iter().map(|s| s.as_bytes())) {
        strings.extend_from_slice(text);
        strings.push(0);
    }
    let len = u16::try_from(strings.len()).expect("strings a u16 can count");
    let mut packet = message::HEADER.to_vec();
    packet.extend_from_slice(&[OPCODE, 0x00, KIND]);
    packet.extend_from_slice(&len.to_le_bytes());
    packet.extend_from_slice(&strings);
    packet
}

/// Made-up stand-ins for fields 1 to 9, laid out as a Pro 3's: the model
/// number, the manufacturer, a serial number, the two firmware versions in
/// Apple's internal form, the hardware version, the updater, and the pods'
/// serial numbers. Nothing here is a real pair's.
#[cfg(any(test, feature = "harness"))]
pub const MADE_UP_FIELDS: [&str; 9] = [
    "A0000",
    "Example Inc.",
    "SERIAL0000",
    "1.0000000000000000.0000",
    "1.0000000000000000.0000",
    "1.0.0",
    "com.example.updater",
    "SERIAL0001",
    "SERIAL0002",
];

/// A device information packet with `name` and `version` as field 10's
/// value, [`MADE_UP_FIELDS`] between them.
#[cfg(any(test, feature = "harness"))]
pub fn info_packet_with_version(name: impl AsRef<[u8]>, version: &str) -> Vec<u8> {
    let mut rest = MADE_UP_FIELDS.to_vec();
    rest.push(version);
    info_packet(name, &rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field 10 as both pairs sent it on 2026-11-02, the iPhone showing
    /// `9A348` for both (#161).
    const SEEN: &str = "9442752";

    fn version(raw: u32) -> String {
        FirmwareVersion::from_raw(raw).to_string()
    }

    /// The length counts what follows it, `04 00` included, as the pods'
    /// `f8 00` did in a 257-byte packet.
    #[test]
    fn info_packet_is_laid_out_as_the_pods_send_it() {
        let packet = info_packet("Pods", &["A0000"]);
        assert_eq!(
            crate::hex(&packet),
            "04 00 04 00 1d 00 02 0d 00 04 00 50 6f 64 73 00 41 30 30 30 30 00"
        );
        assert_eq!(packet.len(), 9 + 0x0d);
    }

    #[test]
    fn reads_the_name_and_the_version() {
        let info = parse_device_info(&info_packet_with_version("Someone’s AirPods Pro 3", SEEN));
        assert_eq!(
            info,
            Ok(DeviceInfo {
                name: Ok("Someone’s AirPods Pro 3".into()),
                version: Ok(FirmwareVersion::from_raw(9_442_752)),
            })
        );
        assert_eq!(
            info_name_bytes(&info_packet_with_version("Pods", SEEN)),
            Some(&b"Pods"[..])
        );
    }

    /// The one value seen, as the iPhone showed it: `0x9015C0`.
    #[test]
    fn the_version_seen_reads_as_the_iphone_shows_it() {
        assert_eq!(version(9_442_752), "9A348");
        assert_eq!(version(0x0090_15C0), "9A348");
    }

    /// Each part where the layout puts it: the major from bit 20 up, the
    /// letter below it, the build in decimal, a suffix letter from the low
    /// nibble, none for 0.
    #[test]
    fn each_part_of_the_version_has_its_place() {
        assert_eq!(version(0x0081_00E0), "8B14");
        assert_eq!(version(0x0000_0000), "0A0");
        assert_eq!(version(0x0015_FFF0), "1F4095");
        assert_eq!(version(0x00A0_0010), "10A1");
        assert_eq!(version(0x0090_15C1), "9A348a");
        assert_eq!(version(0x0090_15CF), "9A348o");
    }

    /// LibrePods' value for the field, which leaves 3 in the low nibble:
    /// read as a suffix letter, the guess for it.
    #[test]
    fn librepods_value_reads_with_a_suffix() {
        assert_eq!(version(8_454_371), "8B14c");
    }

    /// A version that does not read leaves the name, and a name that does
    /// not read leaves the version.
    #[test]
    fn each_field_fails_on_its_own() {
        for bad in [
            "",
            "9A348",
            "-1",
            "+9442752",
            " 9442752",
            "4294967296",
            "0x9015C0",
        ] {
            let info = parse_device_info(&info_packet_with_version("Pods", bad)).unwrap();
            assert_eq!(info.name.as_deref(), Ok("Pods"));
            assert_eq!(info.version, Err(FieldError::NotANumber), "{bad:?}");
        }

        let mut not_utf8 = info_packet_with_version("Pods", SEEN);
        not_utf8[PREAMBLE_LEN] = 0xFF;
        let info = parse_device_info(&not_utf8).unwrap();
        assert_eq!(info.name, Err(FieldError::NotUtf8));
        assert_eq!(info.version, Ok(FirmwareVersion::from_raw(9_442_752)));
    }

    /// A packet that ends before field 10, or before its NUL, has no
    /// version; one that ends before the name's NUL, neither.
    #[test]
    fn a_field_cut_off_is_missing() {
        let short = parse_device_info(&info_packet("Pods", &MADE_UP_FIELDS)).unwrap();
        assert_eq!(short.name.as_deref(), Ok("Pods"));
        assert_eq!(short.version, Err(FieldError::Missing));

        let full = info_packet_with_version("Pods", SEEN);
        let cut = parse_device_info(&full[..full.len() - 1]).unwrap();
        assert_eq!(cut.version, Err(FieldError::Missing));

        let unterminated = &info_packet("Pods", &[])[..13];
        assert_eq!(
            parse_device_info(unterminated),
            Ok(DeviceInfo {
                name: Err(FieldError::Missing),
                version: Err(FieldError::Missing),
            })
        );
        assert_eq!(info_name_bytes(unterminated), None);
    }

    /// What comes after field 10 - binary bytes on the hardware, NULs
    /// among them - is not read.
    #[test]
    fn what_follows_the_version_is_not_read() {
        let mut packet = info_packet_with_version("Pods", SEEN);
        packet.extend_from_slice(&[0x00, 0x01, 0xFF, 0x00, 0x6A]);
        let info = parse_device_info(&packet).unwrap();
        assert_eq!(info.version, Ok(FirmwareVersion::from_raw(9_442_752)));
    }

    #[test]
    fn info_needs_the_opcode() {
        let mut other = info_packet_with_version("Pods", SEEN);
        other[4] = 0x1E;
        assert_eq!(
            parse_device_info(&other),
            Err(InfoParseError::NotInfoPacket)
        );
        let preamble = &info_packet("Pods", &[])[..PREAMBLE_LEN];
        assert!(!is_info_packet(preamble));
        assert_eq!(
            parse_device_info(preamble),
            Err(InfoParseError::NotInfoPacket)
        );
    }

    /// A 0x1D without `02` at byte 6, or without `04 00` ahead of the name,
    /// is no device information packet this module knows, and its bytes are
    /// not read.
    #[test]
    fn info_needs_the_preamble_every_one_seen_had() {
        let mut kind = info_packet_with_version("Pods", SEEN);
        kind[6] = 0x01;
        let mut start = info_packet_with_version("Pods", SEEN);
        start[9] = 0x05;
        let mut start_high = info_packet_with_version("Pods", SEEN);
        start_high[10] = 0x01;
        for packet in [kind, start, start_high] {
            assert!(!is_info_packet(&packet), "{packet:02x?}");
            assert_eq!(info_name_bytes(&packet), None);
            assert_eq!(
                parse_device_info(&packet),
                Err(InfoParseError::NotInfoPacket)
            );
        }
    }

    /// A name cut at 127 bytes inside a character, as the pods would cut a
    /// longer one, is not UTF-8: the bytes are there, the text is not.
    #[test]
    fn a_name_cut_inside_a_character_is_not_text() {
        let long = "ü".repeat(64);
        let packet = info_packet_with_version(&long.as_bytes()[..127], SEEN);
        assert_eq!(info_name_bytes(&packet).map(<[u8]>::len), Some(127));
        let info = parse_device_info(&packet).unwrap();
        assert_eq!(info.name, Err(FieldError::NotUtf8));
        assert!(info.version.is_ok());
    }

    /// As the probes print it: the two fields, the version with field 10's
    /// value beside it.
    #[test]
    fn prints_the_two_fields_alone() {
        let info = parse_device_info(&info_packet_with_version("Pods", SEEN)).unwrap();
        assert_eq!(info.to_string(), r#"name "Pods", firmware 9A348 (9442752)"#);
        let unread = DeviceInfo {
            name: Err(FieldError::NotUtf8),
            version: Err(FieldError::Missing),
        };
        assert_eq!(
            unread.to_string(),
            "name not read: not UTF-8, firmware not read: not in the packet"
        );
    }

    #[test]
    fn an_empty_name_reads_as_empty() {
        let info = parse_device_info(&info_packet_with_version("", SEEN)).unwrap();
        assert_eq!(info.name.as_deref(), Ok(""));
    }
}
