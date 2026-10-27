//! Noise control mode over AAP: sub-command 0x0D of the 0x09 settings family.
//!
//! Format: 04 00 04 00 09 00 [sub-command] [value] 00 00 00
//!
//! The mode packet serves both directions - we send it to switch mode, and the
//! device sends it in its startup dump and whenever the mode is changed from
//! another device. Off additionally needs sub-command 0x34, the setting that
//! permits it at all. See `docs/aap-noise-control.md` for the verified layout
//! and why the response cannot be used as an acknowledgement.

use crate::aap::message::{self, HEADER};

/// Command byte position, shared by every AAP packet.
const CMD: usize = message::OPCODE;
/// Sub-command position within the settings family.
const SUB_CMD: usize = 6;
/// Mode byte position.
const MODE: usize = 7;

/// The settings command family, the opcode a mode report comes under.
pub const OPCODE: u8 = CMD_SETTINGS;
const CMD_SETTINGS: u8 = 0x09;

/// Enables conversational awareness and adaptive transparency. Without it,
/// Adaptive is unavailable - see [`set_packet`].
pub const ENABLE_FEATURES: [u8; 14] = [
    HEADER[0], HEADER[1], HEADER[2], HEADER[3], 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00,
];
/// Noise control within that family. The family carries a dozen other
/// sub-commands, so this has to be checked too - see [`is_noise_mode_packet`].
const SUB_NOISE_CONTROL: u8 = 0x0D;

/// Whether Off is allowed as a listening mode at all. Recent firmware refuses
/// [`NoiseMode::Off`] with an error chime unless this is enabled - see
/// [`allow_off_packet`].
const SUB_ALLOW_OFF: u8 = 0x34;

/// The enable/disable values shared by the boolean settings in this family.
/// Note that disabled is 0x02, not 0x00.
const ENABLED: u8 = 0x01;
const DISABLED: u8 = 0x02;

/// The settings-changed notification, the device's one consistent answer to a
/// mode change - see [`settings_changed_packet`].
#[cfg(any(test, feature = "harness"))]
const CMD_SETTINGS_CHANGED: u8 = 0x4B;

/// The four noise control modes an AirPods pair can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseMode {
    Off,
    NoiseCancelling,
    Transparency,
    Adaptive,
}

impl NoiseMode {
    /// Presentation order, shared by the window and the tray so the two agree.
    /// Deliberately not the order of the wire values.
    pub const ALL: [NoiseMode; 4] = [
        Self::Transparency,
        Self::Adaptive,
        Self::NoiseCancelling,
        Self::Off,
    ];

    /// The mode byte, or `None` for a value outside the four known modes.
    pub fn from_byte(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Off),
            0x02 => Some(Self::NoiseCancelling),
            0x03 => Some(Self::Transparency),
            0x04 => Some(Self::Adaptive),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Self::Off => 0x01,
            Self::NoiseCancelling => 0x02,
            Self::Transparency => 0x03,
            Self::Adaptive => 0x04,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::NoiseCancelling => "Noise Cancelling",
            Self::Transparency => "Transparency",
            Self::Adaptive => "Adaptive",
        }
    }

    /// One-line explanation, used as the subtitle of the window's rows.
    pub fn description(self) -> &'static str {
        match self {
            Self::Off => "Noise control disabled",
            Self::NoiseCancelling => "Block out background noise",
            Self::Transparency => "Hear the world around you",
            Self::Adaptive => "Automatically adjusts to your environment",
        }
    }
}

impl std::fmt::Display for NoiseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoiseParseError {
    #[error("not a noise control packet")]
    NotNoiseControlPacket,
    #[error("unknown noise control mode 0x{0:02X}")]
    UnknownMode(u8),
}

/// One single-byte setting in the 0x09 family, the shape of every command we
/// send in it. Their trailing three bytes are unused; a report can fill them -
/// see `settings_report`.
fn settings_packet(sub_cmd: u8, value: u8) -> [u8; 11] {
    [
        HEADER[0],
        HEADER[1],
        HEADER[2],
        HEADER[3],
        CMD_SETTINGS,
        0x00,
        sub_cmd,
        value,
        0x00,
        0x00,
        0x00,
    ]
}

/// The packet that switches the device to `mode`.
pub fn set_packet(mode: NoiseMode) -> [u8; 11] {
    settings_packet(SUB_NOISE_CONTROL, mode.as_byte())
}

/// The packet that permits, or forbids, Off as a listening mode.
///
/// Recent firmware treats Off as opt-in: without this the AirPods answer
/// `set_packet(NoiseMode::Off)` with an error chime and stay in the mode they
/// were in. Apple gates it because loud sound reduction does not apply with
/// noise control off. It is a persistent device setting - the same switch the
/// user sees in iOS - so enabling it is not something to do unasked.
pub fn allow_off_packet(allowed: bool) -> [u8; 11] {
    settings_packet(SUB_ALLOW_OFF, if allowed { ENABLED } else { DISABLED })
}

/// A setting in the 0x09 family as the device reports it in its startup dump:
/// the value from byte 7 on, zero-padded to the family's 11 bytes. Most values
/// are a single byte; 0x1F's is two.
///
/// A mode report is the same packet as [`set_packet`], which serves both
/// directions.
///
/// # Panics
///
/// With a value of no bytes, or of more than the four the packet has room for.
#[cfg(any(test, feature = "harness"))]
pub fn settings_report(sub_cmd: u8, value: &[u8]) -> [u8; 11] {
    assert!(
        (1..=4).contains(&value.len()),
        "a setting's value is 1 to 4 bytes, got {}",
        value.len()
    );
    let mut packet = settings_packet(sub_cmd, value[0]);
    packet[MODE..MODE + value.len()].copy_from_slice(value);
    packet
}

/// The notification that follows a mode change: `04 00 04 00 4B 00 02 00 01
/// 09`, a count of one and the family that changed.
///
/// Byte-identical whichever mode was set, so it acknowledges that a command
/// was processed and nothing more.
#[cfg(any(test, feature = "harness"))]
pub fn settings_changed_packet() -> [u8; 10] {
    [
        HEADER[0],
        HEADER[1],
        HEADER[2],
        HEADER[3],
        CMD_SETTINGS_CHANGED,
        0x00,
        0x02,
        0x00,
        0x01,
        CMD_SETTINGS,
    ]
}

/// Reads an allow-Off command: `Some(true)` for `0x34 01`, `Some(false)` for
/// `0x34 02`, `None` for anything else - the inverse of [`allow_off_packet`],
/// for a fake on the receiving end.
#[cfg(any(test, feature = "harness"))]
pub fn parse_allow_off_packet(packet: &[u8]) -> Option<bool> {
    if packet.len() <= MODE || packet[CMD] != CMD_SETTINGS || packet[SUB_CMD] != SUB_ALLOW_OFF {
        return None;
    }
    match packet[MODE] {
        ENABLED => Some(true),
        DISABLED => Some(false),
        _ => None,
    }
}

/// True for a mode report: the AAP header, then 0x09 at byte 4 and 0x0D at
/// byte 6.
///
/// Byte 6 matters as much as byte 4: the 0x09 family also carries 0x17, 0x1B,
/// 0x24 and a dozen more sub-commands, all of which arrive in the startup dump.
/// Treating any 0x09 packet as noise control reads an unrelated setting's value
/// as a mode.
pub fn is_noise_mode_packet(packet: &[u8]) -> bool {
    packet.len() > MODE
        && message::opcode(packet) == Some(CMD_SETTINGS)
        && packet[SUB_CMD] == SUB_NOISE_CONTROL
}

/// Reads the mode out of a report.
pub fn parse_noise_mode_packet(packet: &[u8]) -> Result<NoiseMode, NoiseParseError> {
    if !is_noise_mode_packet(packet) {
        return Err(NoiseParseError::NotNoiseControlPacket);
    }
    read(packet)
}

/// [`parse_noise_mode_packet`] past its header check, for a packet
/// [`is_noise_mode_packet`] has accepted.
pub(super) fn read(packet: &[u8]) -> Result<NoiseMode, NoiseParseError> {
    NoiseMode::from_byte(packet[MODE]).ok_or(NoiseParseError::UnknownMode(packet[MODE]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hex from `docs/aap-noise-control.md`, byte for byte.
    #[test]
    fn builds_the_documented_packets() {
        let hex = |p: [u8; 11]| {
            p.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join("")
        };
        assert_eq!(hex(set_packet(NoiseMode::Off)), "0400040009000d01000000");
        assert_eq!(
            hex(set_packet(NoiseMode::NoiseCancelling)),
            "0400040009000d02000000"
        );
        assert_eq!(
            hex(set_packet(NoiseMode::Transparency)),
            "0400040009000d03000000"
        );
        assert_eq!(
            hex(set_packet(NoiseMode::Adaptive)),
            "0400040009000d04000000"
        );
    }

    /// From LibrePods' control command table: 0x34, enabled 0x01 / disabled 0x02.
    #[test]
    fn builds_the_allow_off_packets() {
        assert_eq!(
            allow_off_packet(true),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x34, 0x01, 0x00, 0x00, 0x00
            ]
        );
        assert_eq!(
            allow_off_packet(false),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x34, 0x02, 0x00, 0x00, 0x00
            ]
        );
    }

    /// It shares the 0x09 command byte with a mode report, so a loose check
    /// would read its value byte as a mode - 0x01 being Off, that reads as the
    /// mode the device just refused to enter.
    #[test]
    fn allow_off_is_not_a_mode_report() {
        assert!(!is_noise_mode_packet(&allow_off_packet(true)));
    }

    #[test]
    fn round_trips_every_mode() {
        for mode in NoiseMode::ALL {
            let packet = set_packet(mode);
            assert!(is_noise_mode_packet(&packet));
            assert_eq!(parse_noise_mode_packet(&packet), Ok(mode));
            assert_eq!(NoiseMode::from_byte(mode.as_byte()), Some(mode));
        }
    }

    #[test]
    fn rejects_other_settings_sub_commands() {
        // 0x1F, observed in the startup dump with data 50 50. Validating on the
        // command byte alone would report mode 0x50.
        let other = [0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x1F, 0x50, 0x50];
        assert!(!is_noise_mode_packet(&other));
        assert_eq!(
            parse_noise_mode_packet(&other),
            Err(NoiseParseError::NotNoiseControlPacket)
        );
    }

    #[test]
    fn rejects_the_settings_changed_notification() {
        // 0x4B, the only consistent answer to a mode change - and no use as one,
        // since it is byte-identical whichever mode was set.
        assert!(!is_noise_mode_packet(&settings_changed_packet()));
    }

    /// The hex from `docs/aap-noise-control.md`, byte for byte.
    #[test]
    fn builds_the_documented_notification() {
        let hex: String = settings_changed_packet()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "040004004b0002000109");
    }

    #[test]
    fn a_mode_report_is_the_mode_packet() {
        for mode in NoiseMode::ALL {
            let report = settings_report(SUB_NOISE_CONTROL, &[mode.as_byte()]);
            assert_eq!(report, set_packet(mode));
            assert_eq!(parse_noise_mode_packet(&report), Ok(mode));
        }
    }

    /// 0x1F, from the startup dump in `docs/aap-noise-control.md`: the second
    /// value byte lands at byte 8.
    #[test]
    fn reports_a_two_byte_setting() {
        assert_eq!(
            settings_report(0x1F, &[0x50, 0x50]),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x1F, 0x50, 0x50, 0x00, 0x00
            ]
        );
    }

    #[test]
    fn allow_off_round_trips() {
        assert_eq!(parse_allow_off_packet(&allow_off_packet(true)), Some(true));
        assert_eq!(
            parse_allow_off_packet(&allow_off_packet(false)),
            Some(false)
        );
    }

    /// A mode packet carries 0x01 for Off at the same byte; only byte 6 tells
    /// the two apart.
    #[test]
    fn a_mode_packet_is_not_an_allow_off_command() {
        assert_eq!(parse_allow_off_packet(&set_packet(NoiseMode::Off)), None);
        assert_eq!(
            parse_allow_off_packet(&settings_report(SUB_ALLOW_OFF, &[0x00])),
            None
        );
        assert_eq!(
            parse_allow_off_packet(&allow_off_packet(true)[..MODE]),
            None
        );
    }

    #[test]
    fn rejects_truncated_packets() {
        let full = set_packet(NoiseMode::Adaptive);
        for len in 0..=MODE {
            assert!(!is_noise_mode_packet(&full[..len]), "accepted {len} bytes");
        }
        assert!(is_noise_mode_packet(&full[..=MODE]));
    }

    #[test]
    fn rejects_unknown_mode_bytes() {
        let mut packet = set_packet(NoiseMode::Off);
        packet[MODE] = 0x05;
        assert_eq!(
            parse_noise_mode_packet(&packet),
            Err(NoiseParseError::UnknownMode(0x05))
        );
    }

    #[test]
    fn lists_every_mode_once() {
        for mode in NoiseMode::ALL {
            assert_eq!(
                NoiseMode::ALL.iter().filter(|m| **m == mode).count(),
                1,
                "{mode} listed more than once"
            );
        }
    }
}
