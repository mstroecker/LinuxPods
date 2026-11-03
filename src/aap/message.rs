//! What the device sends, read into one [`Message`] by [`decode`].
//!
//! Every packet after the handshake's answer starts with the same four bytes,
//! [`HEADER`], and names what it carries by its opcode, byte 4. That is
//! checked here, once, and the opcode picks the parser: battery (0x04), the
//! ear report (0x06), the primary pod (0x08), the 0x09 settings family - of
//! which the noise control mode (sub-command 0x0D) and Conversation
//! Awareness (0x28) are read - the proximity keys (0x31) and the speech
//! level (0x4B), and the pods' name and firmware version in the device
//! information packet (0x1D). Each parser checks what is its own beyond
//! that, as it always has.

use crate::aap::battery::{self, BatteryInfo, BatteryParseError};
use crate::aap::conversation::{self, ConversationParseError};
use crate::aap::ear::{self, EarReport};
use crate::aap::info::{self, DeviceInfo};
use crate::aap::keys::{self, KeyParseError, ProximityKey};
use crate::aap::noise::{self, NoiseMode, NoiseParseError};
use crate::ble::parser::PodSide;

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
    /// Where each pod is, the primary pod's place first: once as the link
    /// comes up, and whenever a pod goes into or out of an ear or the case.
    Ear(EarReport),
    /// The primary pod, `None` for a byte naming neither: once as the link
    /// comes up, before the first [`Message::Ear`], and at a role switch.
    Primary(Option<PodSide>),
    /// A noise control mode report: in the startup dump, now and then as the
    /// echo of a mode set here, and when the mode is changed elsewhere.
    NoiseMode(NoiseMode),
    /// Whether Conversation Awareness is on: in the startup dump. Whether
    /// the pods report it when it changes is not known.
    ConversationAwareness(bool),
    /// A speech level, with Conversation Awareness on: while the user
    /// speaks, and around every change of noise mode - see
    /// [`conversation::Speech`].
    SpeechLevel(u8),
    /// The proximity keys, in answer to the key request.
    Keys(Vec<ProximityKey>),
    /// The pods' own name and their firmware version, from the device
    /// information packet at the start of every connection - the rest of
    /// that packet is not read. After a rename, the next connection's
    /// carries the new name. Each field is read on its own, and carries its
    /// own error: the packet as a whole does not fail.
    DeviceInfo(DeviceInfo),
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
    #[error("Conversation Awareness parse error: {0}")]
    ConversationAwareness(#[from] ConversationParseError),
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
        ear::EAR_OPCODE if ear::is_ear_packet(packet) => Message::Ear(ear::read_ear(packet)),
        ear::PRIMARY_OPCODE if ear::is_primary_packet(packet) => {
            Message::Primary(ear::read_primary(packet))
        }
        noise::OPCODE if noise::is_noise_mode_packet(packet) => {
            Message::NoiseMode(noise::read(packet)?)
        }
        conversation::SETTING_OPCODE if conversation::is_setting_packet(packet) => {
            Message::ConversationAwareness(conversation::read_setting(packet)?)
        }
        conversation::LEVEL_OPCODE if conversation::is_level_packet(packet) => {
            Message::SpeechLevel(conversation::read_level(packet))
        }
        keys::OPCODE if keys::is_key_packet(packet) => Message::Keys(keys::read(packet)?),
        info::OPCODE if info::is_info_packet(packet) => Message::DeviceInfo(info::read(packet)),
        _ => Message::Unknown {
            opcode: Some(opcode),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::battery::{Battery, Component, Status, battery_packet};
    use crate::aap::ear::{Place, ear_packet, primary_packet};
    use crate::aap::info::{FieldError, FirmwareVersion};
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

    /// The setting both ways, and a level of each kind the #150 run had.
    #[test]
    fn decodes_conversation_awareness() {
        assert_eq!(
            decode(&conversation::set_packet(true)),
            Ok(Message::ConversationAwareness(true))
        );
        assert_eq!(
            decode(&noise::settings_report(0x28, &[0x02])),
            Ok(Message::ConversationAwareness(false))
        );
        for level in [0x01, 0x02, 0x03, 0x07, 0x08, 0x09] {
            assert_eq!(
                decode(&conversation::level_packet(level)),
                Ok(Message::SpeechLevel(level))
            );
        }
    }

    #[test]
    fn decodes_an_ear_report() {
        let report = EarReport {
            primary: Place::InEar,
            secondary: Place::InClosedCase,
        };
        assert_eq!(decode(&ear_packet(report)), Ok(Message::Ear(report)));
    }

    #[test]
    fn decodes_the_primary_pod() {
        for primary in [PodSide::Left, PodSide::Right] {
            assert_eq!(
                decode(&primary_packet(primary, true, false)),
                Ok(Message::Primary(Some(primary)))
            );
        }
        let mut neither = primary_packet(PodSide::Left, true, false);
        neither[6] = 0x03;
        assert_eq!(decode(&neither), Ok(Message::Primary(None)));
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

    #[test]
    fn decodes_the_device_information() {
        assert_eq!(
            decode(&info::info_packet_with_version("Someone’s Pods", "9442752")),
            Ok(Message::DeviceInfo(DeviceInfo {
                name: Ok("Someone’s Pods".into()),
                version: Ok(FirmwareVersion::from_raw(9_442_752)),
            }))
        );
    }

    /// A field that does not read is the field's error, not the packet's:
    /// the other field comes through.
    #[test]
    fn a_device_information_field_that_does_not_read_leaves_the_other() {
        let mut not_utf8 = info::info_packet_with_version("Pods", "9442752");
        not_utf8[info::PREAMBLE_LEN] = 0xFF;
        assert_eq!(
            decode(&not_utf8),
            Ok(Message::DeviceInfo(DeviceInfo {
                name: Err(FieldError::NotUtf8),
                version: Ok(FirmwareVersion::from_raw(9_442_752)),
            }))
        );
        assert_eq!(
            decode(&info::info_packet("Pods", &["A0000"])),
            Ok(Message::DeviceInfo(DeviceInfo {
                name: Ok("Pods".into()),
                version: Err(FieldError::Missing),
            }))
        );
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

        let mut setting = conversation::set_packet(true);
        setting[7] = 0x00;
        assert_eq!(
            decode(&setting),
            Err(DecodeError::ConversationAwareness(
                ConversationParseError::UnknownValue(0x00)
            ))
        );

        let no_keys = [0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x00];
        assert_eq!(
            decode(&no_keys),
            Err(DecodeError::Keys(KeyParseError::NoKeys))
        );
    }

    /// What the app has no use for is passed over, by its opcode: the startup
    /// dump's other settings, a 0x4B shaped otherwise than a speech level, a
    /// 0x1D shaped otherwise than the device information, the list of known
    /// hosts that comes with the handshake's answer.
    #[test]
    fn other_packets_are_unknown() {
        let mut other_4b = conversation::level_packet(0x09);
        other_4b[8] = 0x02;
        let mut other_1d = info::info_packet("Pods", &["A0000"]);
        other_1d[6] = 0x01;
        let mut other_1d_start = info::info_packet("Pods", &["A0000"]);
        other_1d_start[9] = 0x05;
        let others: [(&[u8], u8); 6] = [
            (&noise::settings_report(0x1F, &[0x50, 0x50]), 0x09),
            (&noise::allow_off_packet(true), 0x09),
            (&other_4b, 0x4B),
            (&other_1d, 0x1D),
            (&other_1d_start, 0x1D),
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
    /// passed them over: no battery count, no key count, no mode byte, no
    /// setting's value, no second place, no primary, no level, no name. A
    /// battery packet needs byte 5 to be zero as well.
    #[test]
    fn a_packet_too_short_for_its_opcode_is_unknown() {
        let short: [&[u8]; 9] = [
            &[0x04, 0x00, 0x04, 0x00, 0x04, 0x00],
            &[0x04, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01],
            &[0x04, 0x00, 0x04, 0x00, 0x31, 0x00],
            &noise::set_packet(NoiseMode::Off)[..7],
            &conversation::set_packet(true)[..7],
            &[0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00],
            &[0x04, 0x00, 0x04, 0x00, 0x08, 0x00],
            &conversation::level_packet(0x01)[..9],
            &info::info_packet("Pods", &[])[..info::PREAMBLE_LEN],
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
    /// used to be recognized by bytes 4 and 6 alone, whatever came before
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
