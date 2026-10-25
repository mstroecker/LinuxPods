//! AAP battery status packet parsing.
//!
//! Format: 04 00 04 00 04 00 [count] ([component] 01 [level] [status] 01)...

/// Which physical component a battery reading belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Right,
    Left,
    Case,
    Unknown(u8),
}

impl From<u8> for Component {
    fn from(v: u8) -> Self {
        match v {
            2 => Self::Right,
            4 => Self::Left,
            8 => Self::Case,
            other => Self::Unknown(other),
        }
    }
}

impl From<Component> for u8 {
    fn from(c: Component) -> Self {
        match c {
            Component::Right => 2,
            Component::Left => 4,
            Component::Case => 8,
            Component::Unknown(other) => other,
        }
    }
}

impl std::fmt::Display for Component {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Right => write!(f, "Right"),
            Self::Left => write!(f, "Left"),
            Self::Case => write!(f, "Case"),
            Self::Unknown(_) => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Charging,
    Discharging,
    /// The part is not reporting - the case while both pods are worn, say. Its
    /// level byte is a placeholder, `0x00` or `0xFF`.
    Disconnected,
    /// A status this parser has no meaning for; the level still counts. `0x05`
    /// was reported once, with the pods in the open case.
    Unknown(u8),
}

impl From<u8> for Status {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Charging,
            2 => Self::Discharging,
            4 => Self::Disconnected,
            other => Self::Unknown(other),
        }
    }
}

impl From<Status> for u8 {
    fn from(s: Status) -> Self {
        match s {
            Status::Charging => 1,
            Status::Discharging => 2,
            Status::Disconnected => 4,
            Status::Unknown(other) => other,
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Charging => write!(f, "Charging"),
            Self::Discharging => write!(f, "Discharging"),
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Unknown(_) => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub component: Component,
    /// `None` when the device reports a level above 100, which means
    /// unavailable, or the part as [`Status::Disconnected`], whatever its level
    /// byte holds.
    pub level: Option<u8>,
    pub status: Status,
}

impl Battery {
    pub fn is_charging(&self) -> bool {
        self.status == Status::Charging
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatteryInfo {
    pub left: Option<Battery>,
    pub right: Option<Battery>,
    pub case: Option<Battery>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BatteryParseError {
    #[error("not a battery packet")]
    NotBatteryPacket,
    #[error("incomplete battery data at offset {0}")]
    Incomplete(usize),
}

/// Everything before the count byte.
const HEADER: [u8; 6] = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00];

/// What [`battery_packet`] writes for a level of `None`. Anything above 100
/// reads as unavailable. A part the device reports as disconnected comes with
/// `0x00` or `0xFF`, read as unknown by its status alone.
#[cfg(any(test, feature = "harness"))]
const LEVEL_UNAVAILABLE: u8 = 0xFF;

/// Header check: 04 00 04 00 04 00 followed by a count byte.
pub fn is_battery_packet(packet: &[u8]) -> bool {
    packet.len() >= 7 && packet[..6] == HEADER
}

pub fn parse_battery_packet(packet: &[u8]) -> Result<BatteryInfo, BatteryParseError> {
    if !is_battery_packet(packet) {
        return Err(BatteryParseError::NotBatteryPacket);
    }

    let count = packet[6] as usize;
    let mut info = BatteryInfo::default();
    let mut offset = 7;

    for _ in 0..count {
        // Each entry is 5 bytes: [component] 01 [level] [status] 01
        if offset + 5 > packet.len() {
            return Err(BatteryParseError::Incomplete(offset));
        }

        let level = packet[offset + 2];
        let status = Status::from(packet[offset + 3]);
        let battery = Battery {
            component: Component::from(packet[offset]),
            // A disconnected part's level is a placeholder, `0x00` or
            // `0xFF`: read as a level, the former showed the case at 0% (#108).
            level: (level <= 100 && status != Status::Disconnected).then_some(level),
            status,
        };

        match battery.component {
            Component::Left => info.left = Some(battery),
            Component::Right => info.right = Some(battery),
            Component::Case => info.case = Some(battery),
            Component::Unknown(_) => {}
        }

        offset += 5;
    }

    Ok(info)
}

/// Builds the packet [`parse_battery_packet`] reads, one entry per battery in
/// the order given. The device lists right, left, case.
///
/// The level goes out as given, so a test can write one above 100, or one
/// beside [`Status::Disconnected`] that the parser reads as `None`; `None` goes
/// out as 0xFF.
///
/// # Panics
///
/// With more than 255 batteries, which the count byte cannot hold.
#[cfg(any(test, feature = "harness"))]
pub fn battery_packet(batteries: &[Battery]) -> Vec<u8> {
    let count = u8::try_from(batteries.len()).expect("at most 255 batteries in a packet");
    let mut packet = HEADER.to_vec();
    packet.push(count);
    for battery in batteries {
        packet.extend_from_slice(&[
            battery.component.into(),
            0x01,
            battery.level.unwrap_or(LEVEL_UNAVAILABLE),
            battery.status.into(),
            0x01,
        ]);
    }
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries as the wire has them - component, level, status - through the
    /// builder.
    fn packet(entries: &[(u8, u8, u8)]) -> Vec<u8> {
        let batteries: Vec<Battery> = entries
            .iter()
            .map(|&(component, level, status)| Battery {
                component: component.into(),
                level: Some(level),
                status: status.into(),
            })
            .collect();
        battery_packet(&batteries)
    }

    /// The AirPods Pro 2 example in LibrePods' `docs/AAP Definitions.md`, byte
    /// for byte - read by its table of components, where 02 is the right pod.
    #[test]
    fn builds_the_librepods_example() {
        let battery = |component, level, status| Battery {
            component,
            level: Some(level),
            status,
        };
        let packet = battery_packet(&[
            battery(Component::Right, 100, Status::Discharging),
            battery(Component::Left, 99, Status::Charging),
            battery(Component::Case, 17, Status::Discharging),
        ]);
        let hex: Vec<String> = packet.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex.join(" "),
            "04 00 04 00 04 00 03 02 01 64 02 01 04 01 63 01 01 08 01 11 02 01"
        );
    }

    #[test]
    fn round_trips_through_the_parser() {
        let info = BatteryInfo {
            right: Some(Battery {
                component: Component::Right,
                level: Some(57),
                status: Status::Charging,
            }),
            left: Some(Battery {
                component: Component::Left,
                level: None,
                status: Status::Disconnected,
            }),
            case: Some(Battery {
                component: Component::Case,
                level: Some(0),
                status: Status::Discharging,
            }),
        };
        let packet = battery_packet(&[info.right.unwrap(), info.left.unwrap(), info.case.unwrap()]);
        assert_eq!(parse_battery_packet(&packet), Ok(info));
    }

    #[test]
    fn wire_values_round_trip() {
        for v in 0..=u8::MAX {
            assert_eq!(u8::from(Component::from(v)), v);
            assert_eq!(u8::from(Status::from(v)), v);
        }
    }

    #[test]
    fn rejects_non_battery_packets() {
        assert!(!is_battery_packet(&[0x04, 0x00]));
        assert!(!is_battery_packet(&[
            0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x01
        ]));
        assert_eq!(
            parse_battery_packet(&[0x00; 7]),
            Err(BatteryParseError::NotBatteryPacket)
        );
    }

    #[test]
    fn parses_all_three_components() {
        // 4 = left, 2 = right, 8 = case; status 1 = charging, 2 = discharging
        let p = packet(&[(4, 80, 2), (2, 75, 1), (8, 42, 2)]);
        let info = parse_battery_packet(&p).unwrap();

        let left = info.left.unwrap();
        assert_eq!(left.level, Some(80));
        assert_eq!(left.status, Status::Discharging);
        assert!(!left.is_charging());

        let right = info.right.unwrap();
        assert_eq!(right.level, Some(75));
        assert!(right.is_charging());

        assert_eq!(info.case.unwrap().level, Some(42));
    }

    /// Levels above 100 mean unavailable, as they do over BLE. The component and
    /// its status still count.
    #[test]
    fn treats_levels_above_100_as_unknown() {
        let info = parse_battery_packet(&packet(&[(4, 100, 2), (2, 101, 1), (8, 255, 2)])).unwrap();
        assert_eq!(info.left.unwrap().level, Some(100));
        let right = info.right.unwrap();
        assert_eq!(right.level, None);
        assert!(right.is_charging());
        assert_eq!(info.case.unwrap().level, None);
    }

    /// Captured on AirPods Pro 3 with both pods worn and the case away: the case
    /// comes as level 0 with status Disconnected, and showed 0% for 53 s (#108).
    #[test]
    fn reads_the_case_away_from_both_worn_pods_as_unknown() {
        let packet = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, 0x02, 0x01, 0x64, 0x02, 0x01, 0x04, 0x01,
            0x64, 0x02, 0x01, 0x08, 0x01, 0x00, 0x04, 0x01,
        ];
        let info = parse_battery_packet(&packet).unwrap();
        let right = info.right.unwrap();
        assert_eq!(
            (right.level, right.status),
            (Some(100), Status::Discharging)
        );
        let left = info.left.unwrap();
        assert_eq!((left.level, left.status), (Some(100), Status::Discharging));
        let case = info.case.unwrap();
        assert_eq!((case.level, case.status), (None, Status::Disconnected));
    }

    /// Captured on AirPods Pro 3 with the right pod worn and the left one in the
    /// case: the case comes as level 0xFF with status Disconnected, while the
    /// pod in it reports charging, at a level that counts (#108).
    #[test]
    fn reads_the_case_holding_one_pod_as_unknown() {
        let packet = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, 0x02, 0x01, 0x64, 0x02, 0x01, 0x04, 0x01,
            0x64, 0x01, 0x01, 0x08, 0x01, 0xFF, 0x04, 0x01,
        ];
        let info = parse_battery_packet(&packet).unwrap();
        let right = info.right.unwrap();
        assert_eq!(
            (right.level, right.status),
            (Some(100), Status::Discharging)
        );
        let left = info.left.unwrap();
        assert_eq!(left.level, Some(100));
        assert!(left.is_charging());
        let case = info.case.unwrap();
        assert_eq!((case.level, case.status), (None, Status::Disconnected));
    }

    /// A disconnected part is unknown whatever its level byte holds - a level
    /// of 0 included, which would otherwise read as an empty battery.
    #[test]
    fn treats_disconnected_parts_as_unknown() {
        for level in [0, 42, 100, 0xFF] {
            let info = parse_battery_packet(&packet(&[(4, level, 4)])).unwrap();
            let left = info.left.unwrap();
            assert_eq!(left.level, None, "level {level:#04x}");
            assert_eq!(left.status, Status::Disconnected);
        }
    }

    /// A status with no known meaning still parses, and leaves the level alone.
    #[test]
    fn keeps_the_level_beside_an_unknown_status() {
        let info = parse_battery_packet(&packet(&[(8, 60, 5)])).unwrap();
        let case = info.case.unwrap();
        assert_eq!((case.level, case.status), (Some(60), Status::Unknown(5)));
        assert!(!case.is_charging());
    }

    #[test]
    fn tolerates_missing_components() {
        let info = parse_battery_packet(&packet(&[(4, 80, 2)])).unwrap();
        assert!(info.left.is_some());
        assert!(info.right.is_none());
        assert!(info.case.is_none());
    }

    #[test]
    fn detects_truncated_entries() {
        let mut p = packet(&[(4, 80, 2), (2, 75, 1)]);
        p.truncate(p.len() - 3); // cut into the second entry
        assert_eq!(
            parse_battery_packet(&p),
            Err(BatteryParseError::Incomplete(12))
        );
    }

    #[test]
    fn ignores_unknown_components() {
        let info = parse_battery_packet(&packet(&[(99, 50, 2)])).unwrap();
        assert_eq!(info, BatteryInfo::default());
    }
}
