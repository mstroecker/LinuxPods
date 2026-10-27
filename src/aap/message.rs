//! What the device sends, read into one [`Message`] by [`decode`].
//!
//! Every packet after the handshake's answer starts with the same four bytes,
//! [`HEADER`], and names what it carries by its opcode, byte 4. That is
//! checked here, once, and the opcode picks the parser: battery (0x04), the
//! 0x09 settings family - of which only the noise control mode (sub-command
//! 0x0D) is read - and the proximity keys (0x31). Each parser checks what is
//! its own beyond that, as it always has.

use crate::aap::battery::{self, BatteryInfo, BatteryParseError};
use crate::aap::keys::{self, KeyParseError, ProximityKey};
use crate::aap::noise::{self, NoiseMode, NoiseParseError};

/// How every packet starts, in both directions, once the handshake is
/// answered. The handshake and its answer are the exceptions: `00 00 04 00`
/// and `01 00 04 00`.
pub const HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];

/// Where a packet carries its opcode.
pub const OPCODE: usize = 4;

/// A packet's opcode, or `None` for one without the [`HEADER`] or too short to
/// carry an opcode at all.
pub fn opcode(packet: &[u8]) -> Option<u8> {
    (packet.len() > OPCODE && packet[..HEADER.len()] == HEADER).then(|| packet[OPCODE])
}

/// A packet from the device, as far as the app reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A battery report: at the start of a session, and whenever a level or
    /// a charging state changes.
    Battery(BatteryInfo),
    /// A noise control mode report: in the startup dump, now and then as the
    /// echo of a mode set here, and when the mode is changed elsewhere.
    NoiseMode(NoiseMode),
    /// The proximity keys, in answer to the key request.
    Keys(Vec<ProximityKey>),
    /// Anything else: another opcode, another setting of the 0x09 family, or
    /// one of the opcodes above too short to be what it names, as the parsers
    /// have always passed those over. `None` for a packet without the
    /// [`HEADER`].
    Unknown { opcode: Option<u8> },
}

/// A packet whose opcode names what it carries, which does not then parse.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("battery parse error: {0}")]
    Battery(#[from] BatteryParseError),
    #[error("noise control parse error: {0}")]
    NoiseMode(#[from] NoiseParseError),
    #[error("key packet rejected: {0}")]
    Keys(#[from] KeyParseError),
}

/// Reads one packet from the device.
pub fn decode(packet: &[u8]) -> Result<Message, DecodeError> {
    let Some(opcode) = opcode(packet) else {
        return Ok(Message::Unknown { opcode: None });
    };
    Ok(match opcode {
        battery::OPCODE if battery::is_battery_packet(packet) => {
            Message::Battery(battery::read(packet)?)
        }
        noise::OPCODE if noise::is_noise_mode_packet(packet) => {
            Message::NoiseMode(noise::read(packet)?)
        }
        keys::OPCODE if keys::is_key_packet(packet) => Message::Keys(keys::read(packet)?),
        _ => Message::Unknown {
            opcode: Some(opcode),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::battery::{Battery, Component, Status, battery_packet};
    use crate::aap::keys::{KeyType, key_packet};

    fn left(level: u8) -> Battery {
        Battery {
            component: Component::Left,
            level: Some(level),
            status: Status::Discharging,
        }
    }

    #[test]
    fn decodes_a_battery_report() {
        let info = decode(&battery_packet(&[left(83)])).unwrap();
        let Message::Battery(info) = info else {
            panic!("{info:?}");
        };
        assert_eq!(info.left, Some(left(83)));
    }

    #[test]
    fn decodes_every_mode_report() {
        for mode in NoiseMode::ALL {
            assert_eq!(
                decode(&noise::set_packet(mode)),
                Ok(Message::NoiseMode(mode))
            );
        }
    }

    #[test]
    fn decodes_the_key_packet() {
        let keys = vec![
            ProximityKey {
                key_type: KeyType::Irk,
                data: vec![0xAA; 16],
            },
            ProximityKey {
                key_type: KeyType::EncKey,
                data: vec![0xBB; 16],
            },
        ];
        assert_eq!(decode(&key_packet(&keys)), Ok(Message::Keys(keys)));
    }

    /// The parsers' errors come through, each under its own kind.
    #[test]
    fn a_packet_that_does_not_parse_is_an_error() {
        let mut truncated = battery_packet(&[left(80), left(75)]);
        truncated.truncate(truncated.len() - 3);
        assert_eq!(
            decode(&truncated),
            Err(DecodeError::Battery(BatteryParseError::Incomplete(12)))
        );

        let mut mode = noise::set_packet(NoiseMode::Off);
        mode[7] = 0x05;
        assert_eq!(
            decode(&mode),
            Err(DecodeError::NoiseMode(NoiseParseError::UnknownMode(0x05)))
        );

        let no_keys = [0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x00];
        assert_eq!(
            decode(&no_keys),
            Err(DecodeError::Keys(KeyParseError::NoKeys))
        );
    }

    /// What the app has no use for is passed over, by its opcode: the startup
    /// dump's other settings, the settings-changed notification, the features
    /// acknowledgement.
    #[test]
    fn other_packets_are_unknown() {
        let others: [(&[u8], u8); 4] = [
            (&noise::settings_report(0x1F, &[0x50, 0x50]), 0x09),
            (&noise::allow_off_packet(true), 0x09),
            (&noise::settings_changed_packet(), 0x4B),
            (&[0x04, 0x00, 0x04, 0x00, 0x2B, 0x00], 0x2B),
        ];
        for (packet, opcode) in others {
            assert_eq!(
                decode(packet),
                Ok(Message::Unknown {
                    opcode: Some(opcode)
                }),
                "{packet:02x?}"
            );
        }
    }

    /// Too short to be what their opcode names, as the parsers have always
    /// passed them over: no battery count, no key count, no mode byte. A
    /// battery packet needs byte 5 to be zero as well.
    #[test]
    fn a_packet_too_short_for_its_opcode_is_unknown() {
        let short: [&[u8]; 4] = [
            &[0x04, 0x00, 0x04, 0x00, 0x04, 0x00],
            &[0x04, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01],
            &[0x04, 0x00, 0x04, 0x00, 0x31, 0x00],
            &noise::set_packet(NoiseMode::Off)[..7],
        ];
        for packet in short {
            assert_eq!(
                decode(packet),
                Ok(Message::Unknown {
                    opcode: Some(packet[OPCODE])
                }),
                "{packet:02x?}"
            );
        }
    }

    /// The header is checked once, for every kind. A noise control report
    /// used to be recognised by bytes 4 and 6 alone, whatever came before
    /// them, while battery and key packets needed the header: a packet of
    /// another kind - shaped as the handshake's answer, say - that carried
    /// 0x09 and 0x0D there would have read as a mode.
    #[test]
    fn a_packet_without_the_header_is_unknown() {
        let mut lookalike = noise::set_packet(NoiseMode::Off);
        lookalike[..4].copy_from_slice(&[0x01, 0x00, 0x04, 0x00]);
        assert!(!noise::is_noise_mode_packet(&lookalike));
        assert_eq!(decode(&lookalike), Ok(Message::Unknown { opcode: None }));

        let mut battery = battery_packet(&[left(83)]);
        battery[0] = 0x05;
        assert_eq!(decode(&battery), Ok(Message::Unknown { opcode: None }));

        for packet in [&[][..], &[0x04], &HEADER, &[0x01, 0x00, 0x04, 0x00, 0x00]] {
            assert_eq!(decode(packet), Ok(Message::Unknown { opcode: None }));
        }
    }
}
