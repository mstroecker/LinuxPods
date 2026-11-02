//! The pods' own name over AAP: the rename packet (opcode 0x1A), and the name
//! in the device information packet (opcode 0x1D) the pods send at the start
//! of every connection.
//!
//! Formats:
//!
//! - rename: `04 00 04 00 1A 00 01 [length] 00 [UTF-8 name]`, sent to rename,
//!   the length in bytes. Nothing answers it.
//! - device information: `04 00 04 00 1D 00`, `02`, a little-endian u16
//!   length of what follows (`f8 00`, 248, in a 257-byte packet), `04 00`,
//!   then NUL-terminated strings, the name first. Only the name is read; the
//!   rest holds the model number and the serial numbers, and is never logged
//!   - see [`crate::aap::keys::redacted_hex`].
//!
//! This is the name the pods give every host, which BlueZ shows as
//! `Device1.Name` - not the alias BlueZ keeps for this computer alone, which
//! is never touched here. See `docs/aap-rename.md`.
//!
//! The rename packet is LibrePods' (`linux/airpods_packets.h`,
//! `AACPManager.createRenamePacket`): both its apps send opcode 0x1A, while its
//! own opcode table lists rename as 0x1E. Confirmed on a Pro 3 on 2026-11-02
//! with `examples/rename_probe` (#160): a second session's 0x1D carried the
//! new name at once, and BlueZ's `Name` after a reconnect. The same run's
//! sweep found the pods keep 127 bytes of a name, any characters, and cut a
//! longer one to its first 127 bytes without a word - see [`MAX_NAME_LEN`].
//! The name's place in 0x1D has been seen on a Pro 3 and a Pro 2 (USB-C). A
//! Pro 2's rename has not been tried, and is assumed to go the same way.

use crate::aap::message::{self, HEADER};

/// The rename opcode, as LibrePods' apps send it.
pub const RENAME_OPCODE: u8 = 0x1A;
/// The device information opcode.
pub const INFO_OPCODE: u8 = 0x1D;

/// The longest name sent, in bytes. A Pro 3 kept every name up to 127 bytes
/// byte for byte, and cut longer ones to their first 127 bytes - inside a
/// character, for one beyond ASCII, which would leave invalid UTF-8 (#160).
/// Most likely a 128-byte buffer with its NUL. Not LibrePods' 32 characters.
pub const MAX_NAME_LEN: usize = 127;

/// What the device information packet carries ahead of its strings: the
/// header, the opcode, `00`, `02`, the u16 length and `04 00`. The `02` and
/// the `04 00` are checked, as every 0x1D seen had them; the length is not.
pub const INFO_PREAMBLE_LEN: usize = 11;

/// Byte 6 of every device information packet seen.
const INFO_KIND: u8 = 0x02;
/// Bytes 9-10 of every device information packet seen, just ahead of the
/// name: counted in the length, unlike what comes before them.
const INFO_STRINGS_START: [u8; 2] = [0x04, 0x00];

/// Why a name cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("the name is empty")]
    Empty,
    #[error("the name is {0} bytes, over the {MAX_NAME_LEN} the AirPods keep")]
    TooLong(usize),
    #[error("the name contains a NUL byte")]
    Nul,
}

/// Why a device information packet's name cannot be read. Never carries the
/// bytes: the rest of the packet holds serial numbers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InfoParseError {
    #[error("not a device information packet")]
    NotInfoPacket,
    #[error("the name in the device information packet is not terminated")]
    Unterminated,
    #[error("the name in the device information packet is not UTF-8")]
    NotUtf8,
}

/// Checks that `name` can be sent: not empty, no NUL, at most
/// [`MAX_NAME_LEN`] bytes.
pub fn check(name: &str) -> Result<(), NameError> {
    let len = name.len();
    if len == 0 {
        return Err(NameError::Empty);
    }
    if len > MAX_NAME_LEN {
        return Err(NameError::TooLong(len));
    }
    if name.contains('\0') {
        return Err(NameError::Nul);
    }
    Ok(())
}

/// The packet that renames the pods to `name`, or why it cannot be sent -
/// see [`check`].
pub fn rename_packet(name: &str) -> Result<Vec<u8>, NameError> {
    check(name)?;
    Ok(rename_packet_unchecked(name.as_bytes()))
}

/// The rename packet for `name` as given, of any length the length byte can
/// hold: for a probe to reach past what the pods keep, as the name run did.
/// The app sends [`rename_packet`]'s. Only the probe's `--sweep` uses it.
///
/// # Panics
///
/// With a name over 255 bytes.
#[doc(hidden)]
pub fn rename_packet_unchecked(name: &[u8]) -> Vec<u8> {
    let len = u8::try_from(name.len()).expect("a name of at most 255 bytes");
    let mut packet = Vec::with_capacity(9 + name.len());
    packet.extend_from_slice(&HEADER);
    packet.extend_from_slice(&[RENAME_OPCODE, 0x00, 0x01, len, 0x00]);
    packet.extend_from_slice(name);
    packet
}

/// True for a device information packet: the AAP header, then 0x1D, `02`
/// at byte 6 and `04 00` at bytes 9-10, and longer than its preamble. Any
/// other is no packet this module knows - a 0x1D shaped otherwise is not
/// read for a name, as a 0x09 report is not read for a mode unless byte 6
/// names the setting.
pub fn is_info_packet(packet: &[u8]) -> bool {
    packet.len() > INFO_PREAMBLE_LEN
        && message::opcode(packet) == Some(INFO_OPCODE)
        && packet[6] == INFO_KIND
        && packet[9..INFO_PREAMBLE_LEN] == INFO_STRINGS_START
}

/// The name's bytes in a device information packet, as sent, UTF-8 or not:
/// `None` for any other packet, or one whose name is unterminated.
pub fn info_name_bytes(packet: &[u8]) -> Option<&[u8]> {
    if !is_info_packet(packet) {
        return None;
    }
    let rest = &packet[INFO_PREAMBLE_LEN..];
    let end = rest.iter().position(|&b| b == 0)?;
    Some(&rest[..end])
}

/// Reads the name out of a device information packet.
pub fn parse_info_name(packet: &[u8]) -> Result<String, InfoParseError> {
    if !is_info_packet(packet) {
        return Err(InfoParseError::NotInfoPacket);
    }
    read_info_name(packet)
}

/// [`parse_info_name`] past its header check, for a packet
/// [`is_info_packet`] has accepted.
pub(super) fn read_info_name(packet: &[u8]) -> Result<String, InfoParseError> {
    let name = info_name_bytes(packet).ok_or(InfoParseError::Unterminated)?;
    String::from_utf8(name.to_vec()).map_err(|_| InfoParseError::NotUtf8)
}

/// A device information packet as the pods send it, laid out as above:
/// `name`, then `rest`, each NUL-terminated. The fake pods' and the tests':
/// `rest` stands in for the model and serial numbers, and is made up. The
/// name is bytes, as the pods hold it: one cut inside a character is not
/// UTF-8.
///
/// # Panics
///
/// With strings longer than the u16 length can count.
#[cfg(any(test, feature = "harness"))]
pub fn info_packet(name: impl AsRef<[u8]>, rest: &[&str]) -> Vec<u8> {
    let mut strings = INFO_STRINGS_START.to_vec();
    for text in std::iter::once(name.as_ref()).chain(rest.iter().map(|s| s.as_bytes())) {
        strings.extend_from_slice(text);
        strings.push(0);
    }
    let len = u16::try_from(strings.len()).expect("strings a u16 can count");
    let mut packet = HEADER.to_vec();
    packet.extend_from_slice(&[INFO_OPCODE, 0x00, INFO_KIND]);
    packet.extend_from_slice(&len.to_le_bytes());
    packet.extend_from_slice(&strings);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Made up, standing in for the model number and serial numbers.
    const REST: [&str; 3] = ["A0000", "Example Inc.", "SERIAL0000"];

    #[test]
    fn rename_packet_as_librepods_sends_it() {
        assert_eq!(
            crate::hex(&rename_packet("Pods").unwrap()),
            "04 00 04 00 1a 00 01 04 00 50 6f 64 73"
        );
    }

    /// The packet the hardware run sent, byte for byte (#160).
    #[test]
    fn rename_packet_as_the_run_sent_it() {
        assert_eq!(
            crate::hex(&rename_packet("Test Pods").unwrap()),
            "04 00 04 00 1a 00 01 09 00 54 65 73 74 20 50 6f 64 73"
        );
    }

    #[test]
    fn rename_length_counts_bytes() {
        // ’ is three bytes in UTF-8.
        let packet = rename_packet("A’s").unwrap();
        assert_eq!(packet[7], 5);
        assert_eq!(&packet[9..], "A’s".as_bytes());
    }

    /// Up to 127 bytes, whatever the characters: the name run kept them all.
    #[test]
    fn rename_takes_names_up_to_what_the_pods_keep() {
        for name in [
            "Müllers Pöds",
            "耳机测试",
            "Pods 🎧",
            "Pods 👩\u{200d}💻",
            "Cafe\u{301} Pods",
            "سماعات",
            "x",
        ] {
            assert_eq!(
                rename_packet(name).map(|p| p[9..].to_vec()),
                Ok(name.into())
            );
        }
        assert_eq!(rename_packet(&"a".repeat(127)).map(|p| p.len()), Ok(136));
    }

    /// Over 127 bytes would be cut by the pods, maybe inside a character, so
    /// it is not sent - counted in bytes, not characters.
    #[test]
    fn rename_refuses_what_cannot_be_sent() {
        assert_eq!(rename_packet(""), Err(NameError::Empty));
        assert_eq!(
            rename_packet(&"a".repeat(128)),
            Err(NameError::TooLong(128))
        );
        // 43 characters, 129 bytes.
        assert_eq!(rename_packet(&"’".repeat(43)), Err(NameError::TooLong(129)));
        // 42 characters, 126 bytes.
        assert!(rename_packet(&"’".repeat(42)).is_ok());
        assert_eq!(rename_packet("a\0b"), Err(NameError::Nul));
        assert_eq!(check("\0"), Err(NameError::Nul));
    }

    /// What the probe's sweep sends past the limit.
    #[test]
    fn the_unchecked_packet_reaches_past_the_limit() {
        let name = "a".repeat(255);
        let packet = rename_packet_unchecked(name.as_bytes());
        assert_eq!(packet[7], 255);
        assert_eq!(packet.len(), 264);
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
    fn info_name_is_the_first_string() {
        assert_eq!(
            parse_info_name(&info_packet("Someone’s AirPods Pro 3", &REST)).as_deref(),
            Ok("Someone’s AirPods Pro 3")
        );
        assert_eq!(
            info_name_bytes(&info_packet("Pods", &REST)),
            Some(&b"Pods"[..])
        );
    }

    #[test]
    fn info_name_needs_the_opcode_and_a_terminator() {
        let mut other = info_packet("Pods", &REST);
        other[4] = 0x1E;
        assert_eq!(parse_info_name(&other), Err(InfoParseError::NotInfoPacket));
        let unterminated = &info_packet("Pods", &[])[..13];
        assert_eq!(
            parse_info_name(unterminated),
            Err(InfoParseError::Unterminated)
        );
        assert_eq!(info_name_bytes(unterminated), None);
        let preamble = &info_packet("Pods", &REST)[..INFO_PREAMBLE_LEN];
        assert!(!is_info_packet(preamble));
        assert_eq!(
            parse_info_name(preamble),
            Err(InfoParseError::NotInfoPacket)
        );
        let mut not_utf8 = info_packet("Pods", &REST);
        not_utf8[INFO_PREAMBLE_LEN] = 0xFF;
        assert_eq!(parse_info_name(&not_utf8), Err(InfoParseError::NotUtf8));
    }

    /// A 0x1D without `02` at byte 6, or without `04 00` ahead of the name,
    /// is no device information packet this module knows, and its bytes are
    /// not read as a name.
    #[test]
    fn info_name_needs_the_preamble_every_one_seen_had() {
        let mut kind = info_packet("Pods", &REST);
        kind[6] = 0x01;
        let mut start = info_packet("Pods", &REST);
        start[9] = 0x05;
        let mut start_high = info_packet("Pods", &REST);
        start_high[10] = 0x01;
        for packet in [kind, start, start_high] {
            assert!(!is_info_packet(&packet), "{packet:02x?}");
            assert_eq!(info_name_bytes(&packet), None);
            assert_eq!(parse_info_name(&packet), Err(InfoParseError::NotInfoPacket));
        }
    }

    /// A name cut at 127 bytes inside a character, as the pods would cut a
    /// longer one, is not UTF-8: the bytes are there, the text is not.
    #[test]
    fn a_name_cut_inside_a_character_is_not_text() {
        let long = "ü".repeat(64);
        let packet = info_packet(&long.as_bytes()[..127], &REST);
        assert_eq!(info_name_bytes(&packet).map(<[u8]>::len), Some(127));
        assert_eq!(parse_info_name(&packet), Err(InfoParseError::NotUtf8));
    }

    #[test]
    fn an_empty_name_reads_as_empty() {
        assert_eq!(parse_info_name(&info_packet("", &REST)).as_deref(), Ok(""));
    }
}
