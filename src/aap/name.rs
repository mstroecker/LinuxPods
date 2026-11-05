//! The pods' own name over AAP: the rename packet (opcode 0x1A). The pods
//! report the name in the device information packet (opcode 0x1D) at the
//! start of every connection, which [`crate::aap::info`] reads.
//!
//! Format: `04 00 04 00 1A 00 01 [length] 00 [UTF-8 name]`, sent to rename,
//! the length in bytes. Nothing answers it.
//!
//! This is the name the pods give every host, which BlueZ shows as
//! `Device1.Name` - not the alias BlueZ keeps for this computer alone, which
//! is never touched here. See `docs/protocol/aap/rename.md`.
//!
//! The rename packet is LibrePods' (`linux/airpods_packets.h`,
//! `AACPManager.createRenamePacket`): both its apps send opcode 0x1A, while its
//! own opcode table lists rename as 0x1E. Confirmed on a Pro 3 on 2026-11-02
//! with `examples/rename_probe` (#160): a second session's 0x1D carried the
//! new name at once, and BlueZ's `Name` after a reconnect. The same run's
//! sweep found the pods keep 127 bytes of a name, any characters, and cut a
//! longer one to its first 127 bytes without a word - see [`MAX_NAME_LEN`].
//! A Pro 2's rename has not been tried, and is assumed to go the same way.

use crate::aap::message::HEADER;

/// The rename opcode, as LibrePods' apps send it.
pub const RENAME_OPCODE: u8 = 0x1A;

/// The longest name sent, in bytes. A Pro 3 kept every name up to 127 bytes
/// byte for byte, and cut longer ones to their first 127 bytes - inside a
/// character, for one beyond ASCII, which would leave invalid UTF-8 (#160).
/// Most likely a 128-byte buffer with its NUL. Not LibrePods' 32 characters.
pub const MAX_NAME_LEN: usize = 127;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
