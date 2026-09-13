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
    Disconnected,
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
    /// `None` when the device reports a level above 100, which means unavailable.
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

/// Header check: 04 00 04 00 04 00 followed by a count byte.
pub fn is_battery_packet(packet: &[u8]) -> bool {
    packet.len() >= 7 && packet[..6] == [0x04, 0x00, 0x04, 0x00, 0x04, 0x00]
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
        let battery = Battery {
            component: Component::from(packet[offset]),
            level: (level <= 100).then_some(level),
            status: Status::from(packet[offset + 3]),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(entries: &[(u8, u8, u8)]) -> Vec<u8> {
        let mut p = vec![0x04, 0x00, 0x04, 0x00, 0x04, 0x00, entries.len() as u8];
        for (component, level, status) in entries {
            p.extend_from_slice(&[*component, 0x01, *level, *status, 0x01]);
        }
        p
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
