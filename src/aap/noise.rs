//! Noise control mode over AAP: sub-command 0x0D of the 0x09 settings family.
//!
//! Format: 04 00 04 00 09 00 0D [mode] 00 00 00
//!
//! The same packet serves both directions - we send it to switch mode, and the
//! device sends it in its startup dump and whenever the mode is changed from
//! another device. See `docs/aap-noise-control.md` for the verified layout and
//! why the response cannot be used as an acknowledgement.

/// Command byte position, shared by every AAP packet.
const CMD: usize = 4;
/// Sub-command position within the settings family.
const SUB_CMD: usize = 6;
/// Mode byte position.
const MODE: usize = 7;

/// The settings command family.
const CMD_SETTINGS: u8 = 0x09;
/// Noise control within that family. The family carries a dozen other
/// sub-commands, so this has to be checked too - see [`is_noise_mode_packet`].
const SUB_NOISE_CONTROL: u8 = 0x0D;

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

/// The packet that switches the device to `mode`.
pub fn set_packet(mode: NoiseMode) -> [u8; 11] {
    [
        0x04,
        0x00,
        0x04,
        0x00,
        CMD_SETTINGS,
        0x00,
        SUB_NOISE_CONTROL,
        mode.as_byte(),
        0x00,
        0x00,
        0x00,
    ]
}

/// True for a mode report.
///
/// Byte 6 matters as much as byte 4: the 0x09 family also carries 0x17, 0x1B,
/// 0x24 and a dozen more sub-commands, all of which arrive in the startup dump.
/// Treating any 0x09 packet as noise control reads an unrelated setting's value
/// as a mode.
pub fn is_noise_mode_packet(packet: &[u8]) -> bool {
    packet.len() > MODE && packet[CMD] == CMD_SETTINGS && packet[SUB_CMD] == SUB_NOISE_CONTROL
}

/// Reads the mode out of a report.
pub fn parse_noise_mode_packet(packet: &[u8]) -> Result<NoiseMode, NoiseParseError> {
    if !is_noise_mode_packet(packet) {
        return Err(NoiseParseError::NotNoiseControlPacket);
    }
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
        let ack = [0x04, 0x00, 0x04, 0x00, 0x4B, 0x00, 0x02, 0x00, 0x01, 0x09];
        assert!(!is_noise_mode_packet(&ack));
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
