//! Conversation Awareness over AAP: the setting, sub-command 0x28 of the 0x09
//! settings family, and the speech levels the pods send while it is on
//! (opcode 0x4B).
//!
//! Formats:
//!
//! - setting: `04 00 04 00 09 00 28 [01 on / 02 off] 00 00 00`, reported in
//!   the startup dump, and sent to change it, as LibrePods has it
//! - level: `04 00 04 00 4B 00 02 00 01 [level]`, sent by the pods
//!
//! With the setting on, the pods tell the host how loud its media should be
//! while the user speaks - see [`Speech`] - and switch themselves to
//! Transparency and back, which they report as ordinary mode reports. The
//! same levels frame every change of noise mode then: a Pro 3 answered each
//! one level 08, the mode report, level 09. The features packet,
//! [`crate::aap::noise::ENABLE_FEATURES`], is what makes the levels come
//! while audio plays, LibrePods says. See `docs/aap-conversation-awareness.md`.
//!
//! The setting's report in the startup dump has been seen on a Pro 3 (#150);
//! that the same packet changes it is LibrePods' word, not yet tried on a
//! pair from here.

use crate::aap::message;
#[cfg(any(test, feature = "harness"))]
use crate::aap::message::HEADER;
use crate::aap::noise;

/// The settings family the setting comes under, as a mode report does.
pub const SETTING_OPCODE: u8 = noise::OPCODE;
/// The speech level's opcode.
pub const LEVEL_OPCODE: u8 = 0x4B;

/// Conversation Awareness within the settings family. The family carries a
/// dozen other sub-commands, so this has to be checked too - see
/// [`is_setting_packet`].
const SUB_CONVERSATION_AWARENESS: u8 = 0x28;
/// Where the settings family carries its sub-command, and the value.
const SUB_CMD: usize = 6;
const VALUE: usize = 7;

/// What a speech level carries between its opcode and the level: byte 5,
/// then a length of two, `00`, and a count of one. Every 0x4B seen so far
/// had them; one that does not is no level this module knows.
const LEVEL_PREFIX: [u8; 4] = [0x00, 0x02, 0x00, 0x01];
/// Where the level is.
const LEVEL: usize = 9;

/// What a speech level tells the host, as the hardware run of #150 reads it
/// (`docs/aap-conversation-awareness.md`). A level is a target for the
/// media's volume, not a step to count: the pods skip levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speech {
    /// `01` and `02`: the user is speaking; lower the volume.
    Speaking,
    /// `03` to `07`: the way back up, any of them skipped. A Pro 3 went
    /// `03 07 08` within 100 ms.
    Easing,
    /// `08`: back to normal volume. Also sent ahead of every report of a
    /// mode change while the setting is on, so it means normal volume, not
    /// that speech ended.
    Normal,
    /// `09`: idle, the pods back in the user's mode. Also sent right after
    /// the startup dump and after every mode change while the setting is on.
    Idle,
}

impl Speech {
    /// What `level` means, `None` for a level outside `01`-`09`.
    pub fn of(level: u8) -> Option<Self> {
        match level {
            0x01 | 0x02 => Some(Self::Speaking),
            0x03..=0x07 => Some(Self::Easing),
            0x08 => Some(Self::Normal),
            0x09 => Some(Self::Idle),
            _ => None,
        }
    }

    /// A word for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Speaking => "speaking",
            Self::Easing => "easing back",
            Self::Normal => "normal",
            Self::Idle => "idle",
        }
    }
}

/// How a level reads in a log: `01 (speaking)`, or `0c (unknown)`.
pub fn describe_level(level: u8) -> String {
    let meaning = Speech::of(level).map_or("unknown", Speech::label);
    format!("{level:02x} ({meaning})")
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConversationParseError {
    #[error("not a Conversation Awareness packet")]
    NotConversationAwarenessPacket,
    #[error("unknown Conversation Awareness value 0x{0:02X}")]
    UnknownValue(u8),
}

/// The packet that turns Conversation Awareness on or off, as LibrePods has
/// it: whether the pods take it is still to be checked (#150). A persistent
/// setting of the pods', most likely synced to the user's Apple devices as
/// the allow-Off setting (`0x34`) is.
pub fn set_packet(enabled: bool) -> [u8; 11] {
    let value = if enabled {
        noise::ENABLED
    } else {
        noise::DISABLED
    };
    noise::settings_packet(SUB_CONVERSATION_AWARENESS, value)
}

/// True for the setting, sent or reported: the AAP header, then 0x09 at
/// byte 4 and 0x28 at byte 6. Byte 6 matters as much as byte 4: the family's
/// other settings, the noise control mode among them, share byte 4.
pub fn is_setting_packet(packet: &[u8]) -> bool {
    packet.len() > VALUE
        && message::opcode(packet) == Some(SETTING_OPCODE)
        && packet[SUB_CMD] == SUB_CONVERSATION_AWARENESS
}

/// Reads the setting out of a report: on or off.
pub fn parse_setting_packet(packet: &[u8]) -> Result<bool, ConversationParseError> {
    if !is_setting_packet(packet) {
        return Err(ConversationParseError::NotConversationAwarenessPacket);
    }
    read_setting(packet)
}

/// [`parse_setting_packet`] past its header check, for a packet
/// [`is_setting_packet`] has accepted.
pub(super) fn read_setting(packet: &[u8]) -> Result<bool, ConversationParseError> {
    match packet[VALUE] {
        noise::ENABLED => Ok(true),
        noise::DISABLED => Ok(false),
        other => Err(ConversationParseError::UnknownValue(other)),
    }
}

/// True for a speech level: the AAP header, 0x4B at byte 4, and `00 02 00
/// 01` before the level. A 0x4B shaped otherwise is no level.
pub fn is_level_packet(packet: &[u8]) -> bool {
    packet.len() > LEVEL
        && message::opcode(packet) == Some(LEVEL_OPCODE)
        && packet[message::OPCODE + 1..LEVEL] == LEVEL_PREFIX
}

/// Reads the level out of a speech level packet, any byte: [`Speech::of`]
/// says what it means, if anything.
pub fn parse_level_packet(packet: &[u8]) -> Result<u8, ConversationParseError> {
    if !is_level_packet(packet) {
        return Err(ConversationParseError::NotConversationAwarenessPacket);
    }
    Ok(read_level(packet))
}

/// [`parse_level_packet`] past its header check, for a packet
/// [`is_level_packet`] has accepted.
pub(super) fn read_level(packet: &[u8]) -> u8 {
    packet[LEVEL]
}

/// The speech level packet carrying `level`, as the pods send it.
#[cfg(any(test, feature = "harness"))]
pub fn level_packet(level: u8) -> [u8; 10] {
    let [b5, b6, b7, b8] = LEVEL_PREFIX;
    [
        HEADER[0],
        HEADER[1],
        HEADER[2],
        HEADER[3],
        LEVEL_OPCODE,
        b5,
        b6,
        b7,
        b8,
        level,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::noise::{NoiseMode, is_noise_mode_packet, settings_report};

    /// LibrePods' control command table: 0x28, enabled 0x01, disabled 0x02.
    #[test]
    fn builds_the_setting_packets() {
        assert_eq!(
            crate::hex(&set_packet(true)),
            "04 00 04 00 09 00 28 01 00 00 00"
        );
        assert_eq!(
            crate::hex(&set_packet(false)),
            "04 00 04 00 09 00 28 02 00 00 00"
        );
    }

    /// The report in the startup dump is the packet that sets it.
    #[test]
    fn reads_the_setting_both_ways() {
        assert_eq!(parse_setting_packet(&set_packet(true)), Ok(true));
        assert_eq!(parse_setting_packet(&set_packet(false)), Ok(false));
        assert_eq!(
            parse_setting_packet(&settings_report(0x28, &[0x01])),
            Ok(true)
        );
        assert_eq!(
            parse_setting_packet(&settings_report(0x28, &[0x00])),
            Err(ConversationParseError::UnknownValue(0x00))
        );
    }

    /// The setting and a mode report share byte 4, and their values read
    /// alike - 0x01 is on, and Off - so only byte 6 tells them apart.
    #[test]
    fn the_setting_is_not_a_mode_report_nor_a_mode_the_setting() {
        assert!(!is_noise_mode_packet(&set_packet(true)));
        let off = noise::set_packet(NoiseMode::Off);
        assert!(!is_setting_packet(&off));
        assert_eq!(
            parse_setting_packet(&off),
            Err(ConversationParseError::NotConversationAwarenessPacket)
        );
        // 0x26, whose 02 sits where the setting's off does.
        assert!(!is_setting_packet(&settings_report(0x26, &[0x02])));
    }

    #[test]
    fn rejects_a_truncated_setting() {
        let full = set_packet(true);
        for len in 0..=VALUE {
            assert!(!is_setting_packet(&full[..len]), "accepted {len} bytes");
        }
        assert!(is_setting_packet(&full[..=VALUE]));
    }

    /// The bytes of the #150 run, and of the 0x4B AirPods 4 sent after every
    /// mode change (`docs/aap-noise-control.md`), which is level 09.
    #[test]
    fn reads_the_levels_of_the_hardware_run() {
        assert_eq!(
            crate::hex(&level_packet(0x09)),
            "04 00 04 00 4b 00 02 00 01 09"
        );
        let airpods_4 = [0x04, 0x00, 0x04, 0x00, 0x4B, 0x00, 0x02, 0x00, 0x01, 0x09];
        assert_eq!(parse_level_packet(&airpods_4), Ok(0x09));
        for level in [0x01, 0x02, 0x03, 0x06, 0x07, 0x08, 0x09] {
            assert_eq!(parse_level_packet(&level_packet(level)), Ok(level));
        }
    }

    /// A 0x4B shaped otherwise - another length, another count - is not
    /// read as a level, nor is one cut short.
    #[test]
    fn rejects_another_0x4b() {
        let level = level_packet(0x01);
        for byte in 5..LEVEL {
            let mut other = level;
            other[byte] ^= 0x10;
            assert!(!is_level_packet(&other), "byte {byte} changed");
        }
        for len in 0..=LEVEL {
            assert!(!is_level_packet(&level[..len]), "accepted {len} bytes");
        }
        let mut no_header = level;
        no_header[0] = 0x01;
        assert!(!is_level_packet(&no_header));
        assert_eq!(
            parse_level_packet(&set_packet(true)),
            Err(ConversationParseError::NotConversationAwarenessPacket)
        );
    }

    /// As the #150 run reads them: 01 and 02 speaking, 03 to 07 on the way
    /// back, 08 back, 09 idle.
    #[test]
    fn says_what_each_level_means() {
        let meanings: Vec<_> = (0x00..=0x0A).map(Speech::of).collect();
        assert_eq!(
            meanings,
            [
                None,
                Some(Speech::Speaking),
                Some(Speech::Speaking),
                Some(Speech::Easing),
                Some(Speech::Easing),
                Some(Speech::Easing),
                Some(Speech::Easing),
                Some(Speech::Easing),
                Some(Speech::Normal),
                Some(Speech::Idle),
                None,
            ]
        );
        assert_eq!(describe_level(0x01), "01 (speaking)");
        assert_eq!(describe_level(0x0C), "0c (unknown)");
    }
}
