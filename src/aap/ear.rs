//! Ear detection over AAP: where each pod is (opcode 0x06), and which of the
//! two is primary (opcode 0x08).
//!
//! Formats:
//!
//! - ear report: `04 00 04 00 06 00 [primary's place] [secondary's place]`
//! - primary pod: `04 00 04 00 08 00 [primary] 00 [SS] [TT]`
//!
//! Both come unasked, on every change, and once each as the link comes up,
//! 0x08 first. The ear report names no pod: it gives the primary's place
//! first, so it reads only with the primary pod beside it - see
//! [`EarState`]. See `docs/protocol/aap/ear.md` for the capture behind the
//! layouts.

use std::time::{Duration, Instant};

use crate::aap::message;
#[cfg(any(test, feature = "harness"))]
use crate::aap::message::HEADER;
use crate::ble::parser::PodSide;

/// The ear report's opcode.
pub const EAR_OPCODE: u8 = 0x06;
/// The primary pod's opcode.
pub const PRIMARY_OPCODE: u8 = 0x08;

/// Where an ear report carries the primary pod's place, and then the
/// secondary's.
const PRIMARY_PLACE: usize = 6;
const SECONDARY_PLACE: usize = 7;

/// Where the primary pod packet names the primary.
const PRIMARY: usize = 6;

/// The primary pod's byte for each pod.
const LEFT: u8 = 0x01;
const RIGHT: u8 = 0x02;

/// Where a pod is, as an ear report gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `00`.
    InEar,
    /// `01`: out of the ear, and out of the case.
    Out,
    /// `02`: in the case, the lid open.
    InCase,
    /// `03`: in the closed case. Came about 4 s after the lid closed on the
    /// pod, which the battery packet then reports as Disconnected.
    InClosedCase,
    /// Any other byte. Not taken for out: nothing says what it is.
    Unknown(u8),
}

impl Place {
    pub fn from_byte(value: u8) -> Self {
        match value {
            0x00 => Self::InEar,
            0x01 => Self::Out,
            0x02 => Self::InCase,
            0x03 => Self::InClosedCase,
            other => Self::Unknown(other),
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Self::InEar => 0x00,
            Self::Out => 0x01,
            Self::InCase => 0x02,
            Self::InClosedCase => 0x03,
            Self::Unknown(other) => other,
        }
    }

    /// Whether the pod is in an ear: in the case counts as out of it, and
    /// `None` for a place this module has no meaning for.
    pub fn in_ear(self) -> Option<bool> {
        match self {
            Self::InEar => Some(true),
            Self::Out | Self::InCase | Self::InClosedCase => Some(false),
            Self::Unknown(_) => None,
        }
    }

    /// Whether the pod is in the case, its lid open or closed.
    pub fn in_case(self) -> bool {
        matches!(self, Self::InCase | Self::InClosedCase)
    }
}

/// An ear report: both pods' places, the primary pod's first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarReport {
    pub primary: Place,
    pub secondary: Place,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EarParseError {
    #[error("not an ear report")]
    NotEarPacket,
    #[error("not a primary pod packet")]
    NotPrimaryPacket,
}

/// True for an ear report: the AAP header, opcode 0x06, and both places.
pub fn is_ear_packet(packet: &[u8]) -> bool {
    packet.len() > SECONDARY_PLACE && message::opcode(packet) == Some(EAR_OPCODE)
}

/// Reads both pods' places out of an ear report.
pub fn parse_ear(packet: &[u8]) -> Result<EarReport, EarParseError> {
    if !is_ear_packet(packet) {
        return Err(EarParseError::NotEarPacket);
    }
    Ok(read_ear(packet))
}

/// [`parse_ear`] past its header check, for a packet [`is_ear_packet`] has
/// accepted.
pub(super) fn read_ear(packet: &[u8]) -> EarReport {
    EarReport {
        primary: Place::from_byte(packet[PRIMARY_PLACE]),
        secondary: Place::from_byte(packet[SECONDARY_PLACE]),
    }
}

/// True for a primary pod packet: the AAP header, opcode 0x08, and the
/// primary's byte. The three bytes after it are not read, so they need not
/// be there.
pub fn is_primary_packet(packet: &[u8]) -> bool {
    packet.len() > PRIMARY && message::opcode(packet) == Some(PRIMARY_OPCODE)
}

/// Reads the primary pod out of its packet: `None` for a byte other than
/// left's or right's.
pub fn parse_primary(packet: &[u8]) -> Result<Option<PodSide>, EarParseError> {
    if !is_primary_packet(packet) {
        return Err(EarParseError::NotPrimaryPacket);
    }
    Ok(read_primary(packet))
}

/// [`parse_primary`] past its header check, for a packet
/// [`is_primary_packet`] has accepted.
pub(super) fn read_primary(packet: &[u8]) -> Option<PodSide> {
    match packet[PRIMARY] {
        LEFT => Some(PodSide::Left),
        RIGHT => Some(PodSide::Right),
        _ => None,
    }
}

/// The packet [`parse_ear`] reads.
#[cfg(any(test, feature = "harness"))]
pub fn ear_packet(report: EarReport) -> [u8; 8] {
    [
        HEADER[0],
        HEADER[1],
        HEADER[2],
        HEADER[3],
        EAR_OPCODE,
        0x00,
        report.primary.as_byte(),
        report.secondary.as_byte(),
    ]
}

/// The packet [`parse_primary`] reads, as the device sends it: `secondary`
/// is `SS`, whether the secondary pod can be reached - `01`, or `00` with it
/// in the closed case - and `switching` is `TT`, `01` in the first of the
/// two packets a role switch brings.
#[cfg(any(test, feature = "harness"))]
pub fn primary_packet(primary: PodSide, secondary: bool, switching: bool) -> [u8; 10] {
    [
        HEADER[0],
        HEADER[1],
        HEADER[2],
        HEADER[3],
        PRIMARY_OPCODE,
        0x00,
        match primary {
            PodSide::Left => LEFT,
            PodSide::Right => RIGHT,
        },
        0x00,
        u8::from(secondary),
        u8::from(switching),
    ]
}

/// How long an ear report taken for the first half of a role switch waits
/// for the primary pod packet that completes it - see [`EarState`]. The
/// packet came 0.2-31 ms after the report on a Pro 3 in the #142 captures,
/// 0-54 ms in #71's capture 4, and 0.5-87 ms on a Pro 2 (USB-C) on 2026-11-02
/// (#71).
pub const ROLE_SWITCH_HOLD: Duration = Duration::from_millis(250);

/// Which pod is in an ear, as a link's ear reports and primary pod packets
/// tell it.
///
/// An ear report gives the primary pod's place first, so it reads only with
/// the primary: neither is any use alone. The primary is the link's own, never
/// the advertisement's, which lags as the advertisement's in-ear bits do: a
/// connected pair's payload changes seldom, and 4 reached the app in 70 s
/// (#142).
///
/// # The role switch
///
/// When the primary pod comes out, the pod still worn becomes primary about a
/// second later, and the pair sends the ear report in the new order before
/// the primary pod packet that names the new primary, by up to the gaps
/// [`ROLE_SWITCH_HOLD`] records. Read with the old primary in between, the
/// report has the two pods swapped: the one just taken out in an ear, the one
/// still worn out. Broadcast, that swap reached a watcher shaped like the
/// window's render loop - which wakes at the first change, then pauses
/// 100 ms - at nearly every switch in a pipeline test.
///
/// So a report that puts the primary pod in an ear and the secondary out,
/// where the last one had the primary out and the secondary in, is held back
/// rather than taken: until the next primary pod packet, which it is then
/// read with; until another report, which replaces it; or for the hold,
/// [`ROLE_SWITCH_HOLD`] unless made [`EarState::with_hold`], after which it
/// is taken as it is - both pods may have been swapped by hand after all.
///
/// Pure: the time comes in with each call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarState {
    /// The last ear report taken.
    report: Option<EarReport>,
    /// The last primary pod packet's: `None` until one comes, and for one
    /// naming neither pod.
    primary: Option<PodSide>,
    /// A report held back as the first half of a role switch, and when it
    /// came.
    held: Option<(EarReport, Instant)>,
    /// How long a report is held at most.
    hold: Duration,
}

impl Default for EarState {
    fn default() -> Self {
        Self::with_hold(ROLE_SWITCH_HOLD)
    }
}

impl EarState {
    /// Nothing known yet, a report held for a role switch for `hold` at
    /// most. The app's is [`ROLE_SWITCH_HOLD`]; a test on a loaded machine
    /// may want longer, for a switch's primary pod packet never to come too
    /// late for it.
    pub fn with_hold(hold: Duration) -> Self {
        Self {
            report: None,
            primary: None,
            held: None,
            hold,
        }
    }

    /// Takes an ear report that came at `now`, unless it is held back as the
    /// first half of a role switch. Either way, it replaces one held.
    pub fn report(&mut self, report: EarReport, now: Instant) {
        self.held = None;
        if self.looks_like_a_switch(report) {
            self.held = Some((report, now));
        } else {
            self.report = Some(report);
        }
    }

    /// Takes the primary pod, and with it the report held for a role switch,
    /// if one is.
    pub fn set_primary(&mut self, primary: Option<PodSide>) {
        self.primary = primary;
        if let Some((report, _)) = self.held.take() {
            self.report = Some(report);
        }
    }

    /// Takes the report held for a role switch once the hold has passed by
    /// `now` with no primary pod packet, returning whether it did.
    pub fn release(&mut self, now: Instant) -> bool {
        match self.held {
            Some((report, at)) if now.saturating_duration_since(at) >= self.hold => {
                self.held = None;
                self.report = Some(report);
                true
            }
            _ => false,
        }
    }

    /// When the report held for a role switch is to be taken, if one is.
    pub fn held_until(&self) -> Option<Instant> {
        self.held.map(|(_, at)| at + self.hold)
    }

    /// Where each pod is, left then right: `None` until both an ear report
    /// and a primary pod are known.
    pub fn places(&self) -> Option<(Place, Place)> {
        let (report, primary) = (self.report?, self.primary?);
        Some(match primary {
            PodSide::Left => (report.primary, report.secondary),
            PodSide::Right => (report.secondary, report.primary),
        })
    }

    /// Whether each pod is in an ear, left then right: `None` until both an
    /// ear report and a primary pod are known, and `None` for a pod whose
    /// place is unknown.
    pub fn in_ear(&self) -> Option<(Option<bool>, Option<bool>)> {
        let (left, right) = self.places()?;
        Some((left.in_ear(), right.in_ear()))
    }

    /// Whether `report` swaps the pods the last one gave: the primary out
    /// and the secondary in, then the other way round - which is how the
    /// first half of a role switch reads.
    fn looks_like_a_switch(&self, report: EarReport) -> bool {
        let Some(last) = self.report.filter(|_| self.primary.is_some()) else {
            return false;
        };
        let worn = |place: Place| place.in_ear();
        (worn(last.primary), worn(last.secondary)) == (Some(false), Some(true))
            && (worn(report.primary), worn(report.secondary)) == (Some(true), Some(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(primary: u8, secondary: u8) -> EarReport {
        EarReport {
            primary: Place::from_byte(primary),
            secondary: Place::from_byte(secondary),
        }
    }

    /// The capture's packets, byte for byte: the secondary pod out, and in the
    /// closed case.
    #[test]
    fn reads_the_captured_ear_reports() {
        let out = [0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x01];
        assert!(is_ear_packet(&out));
        assert_eq!(
            parse_ear(&out),
            Ok(EarReport {
                primary: Place::InEar,
                secondary: Place::Out
            })
        );
        let closed = [0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x03];
        assert_eq!(parse_ear(&closed).unwrap().secondary, Place::InClosedCase);
    }

    #[test]
    fn reads_every_place() {
        let places = [
            (0x00, Place::InEar, Some(true)),
            (0x01, Place::Out, Some(false)),
            (0x02, Place::InCase, Some(false)),
            (0x03, Place::InClosedCase, Some(false)),
        ];
        for (byte, place, in_ear) in places {
            assert_eq!(Place::from_byte(byte), place);
            assert_eq!(place.as_byte(), byte);
            assert_eq!(place.in_ear(), in_ear, "{place:?}");
            assert_eq!(
                parse_ear(&ear_packet(report(byte, byte))),
                Ok(report(byte, byte))
            );
        }
    }

    /// A place this module has no meaning for is unknown, never out: taken
    /// for out, a pod in an ear would lose its icon to a new firmware's value.
    #[test]
    fn an_unknown_place_is_unknown_not_out() {
        for byte in [0x04, 0x05, 0x7F, 0xFF] {
            let place = Place::from_byte(byte);
            assert_eq!(place, Place::Unknown(byte));
            assert_eq!(place.as_byte(), byte);
            assert_eq!(place.in_ear(), None);
        }
        assert_eq!(
            parse_ear(&ear_packet(report(0x00, 0x04))),
            Ok(EarReport {
                primary: Place::InEar,
                secondary: Place::Unknown(0x04)
            })
        );
    }

    /// The capture's packets, byte for byte: at a connect, the first of a
    /// switch's two, and with the secondary in the closed case.
    #[test]
    fn reads_the_captured_primary_packets() {
        let captured: [([u8; 4], PodSide); 3] = [
            ([0x01, 0x00, 0x01, 0x00], PodSide::Left),
            ([0x02, 0x00, 0x01, 0x01], PodSide::Right),
            ([0x02, 0x00, 0x00, 0x00], PodSide::Right),
        ];
        for (tail, primary) in captured {
            let mut packet = vec![0x04, 0x00, 0x04, 0x00, 0x08, 0x00];
            packet.extend_from_slice(&tail);
            assert!(is_primary_packet(&packet));
            assert_eq!(parse_primary(&packet), Ok(Some(primary)), "{packet:02x?}");
        }
    }

    #[test]
    fn builds_the_captured_primary_packets() {
        assert_eq!(
            crate::hex(&primary_packet(PodSide::Left, true, false)),
            "04 00 04 00 08 00 01 00 01 00"
        );
        assert_eq!(
            crate::hex(&primary_packet(PodSide::Right, true, true)),
            "04 00 04 00 08 00 02 00 01 01"
        );
        assert_eq!(
            crate::hex(&primary_packet(PodSide::Right, false, false)),
            "04 00 04 00 08 00 02 00 00 00"
        );
    }

    /// Bytes 7-9 are not needed to map the pods, and any value there goes:
    /// what they mean is a reading of one capture.
    #[test]
    fn any_value_after_the_primary_goes() {
        for tail in [[0x00; 3], [0xFF; 3], [0x01, 0x02, 0x03]] {
            let mut packet = primary_packet(PodSide::Right, true, false);
            packet[7..].copy_from_slice(&tail);
            assert_eq!(parse_primary(&packet), Ok(Some(PodSide::Right)));
        }
        let short = &primary_packet(PodSide::Left, true, false)[..=PRIMARY];
        assert_eq!(parse_primary(short), Ok(Some(PodSide::Left)));
    }

    #[test]
    fn a_primary_neither_left_nor_right_is_unknown() {
        for byte in [0x00, 0x03, 0x04, 0xFF] {
            let mut packet = primary_packet(PodSide::Left, true, false);
            packet[PRIMARY] = byte;
            assert_eq!(parse_primary(&packet), Ok(None), "{byte:02x}");
        }
    }

    #[test]
    fn rejects_truncated_packets() {
        let ear = ear_packet(report(0x00, 0x01));
        for len in 0..=SECONDARY_PLACE {
            assert!(!is_ear_packet(&ear[..len]), "accepted {len} bytes");
            assert_eq!(parse_ear(&ear[..len]), Err(EarParseError::NotEarPacket));
        }
        let primary = primary_packet(PodSide::Left, true, false);
        for len in 0..=PRIMARY {
            assert!(!is_primary_packet(&primary[..len]), "accepted {len} bytes");
            assert_eq!(
                parse_primary(&primary[..len]),
                Err(EarParseError::NotPrimaryPacket)
            );
        }
    }

    /// Each checks its own header and opcode: neither reads the other, nor a
    /// packet of another kind that carries its opcode's byte elsewhere.
    #[test]
    fn rejects_other_packets() {
        let ear = ear_packet(report(0x00, 0x01));
        let primary = primary_packet(PodSide::Right, true, false);
        assert!(!is_primary_packet(&ear));
        assert!(!is_ear_packet(&primary));

        let mut lookalike = ear;
        lookalike[..4].copy_from_slice(&[0x01, 0x00, 0x04, 0x00]);
        assert!(!is_ear_packet(&lookalike));
        let mut lookalike = primary;
        lookalike[0] = 0x05;
        assert!(!is_primary_packet(&lookalike));

        let battery = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x06, 0x08];
        assert!(!is_ear_packet(&battery) && !is_primary_packet(&battery));
    }

    fn ear_state(primary: PodSide, primary_place: u8, secondary_place: u8) -> EarState {
        let mut state = EarState::default();
        state.set_primary(Some(primary));
        state.report(report(primary_place, secondary_place), Instant::now());
        state
    }

    /// The report gives the primary pod's place first: with the right pod
    /// primary, the first place is the right pod's.
    #[test]
    fn maps_the_places_by_the_primary_pod() {
        assert_eq!(
            ear_state(PodSide::Left, 0x00, 0x01).in_ear(),
            Some((Some(true), Some(false)))
        );
        assert_eq!(
            ear_state(PodSide::Right, 0x00, 0x01).in_ear(),
            Some((Some(false), Some(true)))
        );
        assert_eq!(
            ear_state(PodSide::Right, 0x02, 0x03).in_ear(),
            Some((Some(false), Some(false)))
        );
        assert_eq!(
            ear_state(PodSide::Right, 0x02, 0x03).places(),
            Some((Place::InClosedCase, Place::InCase))
        );
        assert_eq!(
            ear_state(PodSide::Left, 0x00, 0x01).places(),
            Some((Place::InEar, Place::Out))
        );
    }

    /// In the case is the lid open or closed; out of the ear and out of the
    /// case, or a place unknown, is not.
    #[test]
    fn in_the_case_is_either_case_place() {
        for (byte, in_case) in [(0x00, false), (0x01, false), (0x02, true), (0x03, true)] {
            assert_eq!(Place::from_byte(byte).in_case(), in_case, "{byte:02x}");
        }
        assert!(!Place::Unknown(0x04).in_case());
    }

    /// Neither is any use alone: an ear report without the primary pod could
    /// be either way round.
    #[test]
    fn needs_both_a_report_and_the_primary() {
        let now = Instant::now();
        let mut state = EarState::default();
        assert_eq!(state.in_ear(), None);
        state.report(report(0x00, 0x01), now);
        assert_eq!(state.in_ear(), None, "no primary yet");

        let mut state = EarState::default();
        state.set_primary(Some(PodSide::Left));
        assert_eq!(state.in_ear(), None, "no report yet");
        state.report(report(0x00, 0x01), now);
        assert_eq!(state.in_ear(), Some((Some(true), Some(false))));

        state.set_primary(None);
        assert_eq!(state.in_ear(), None, "a primary neither left nor right");
    }

    #[test]
    fn an_unknown_place_is_unknown_for_its_pod_alone() {
        assert_eq!(
            ear_state(PodSide::Right, 0x00, 0x09).in_ear(),
            Some((None, Some(true)))
        );
    }

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// The #142 capture at 17:04:19: the left pod, primary, out; 0.9 s
    /// later the report in the new order, and 3 ms after it the primary pod
    /// packet naming the right pod. The left pod stays out, the right one in.
    #[test]
    fn a_role_switch_never_shows_the_pods_swapped() {
        let start = Instant::now();
        let mut state = ear_state(PodSide::Left, 0x00, 0x00);
        state.report(report(0x01, 0x00), start);
        let left_out = Some((Some(false), Some(true)));
        assert_eq!(state.in_ear(), left_out);

        let switched = start + ms(900);
        state.report(report(0x00, 0x01), switched);
        assert_eq!(state.in_ear(), left_out, "held for the primary pod");
        assert_eq!(state.held_until(), Some(switched + ROLE_SWITCH_HOLD));
        state.set_primary(Some(PodSide::Right));
        assert_eq!(state.in_ear(), left_out);
        assert_eq!(state.held_until(), None);

        // The switch's second primary pod packet, and the left pod back in.
        state.set_primary(Some(PodSide::Right));
        state.report(report(0x00, 0x00), switched + ms(5000));
        assert_eq!(state.in_ear(), Some((Some(true), Some(true))));
    }

    /// Into the case rather than out: the report in the new order puts the
    /// old primary in the case, and is held the same.
    #[test]
    fn a_switch_to_the_case_is_held_too() {
        let now = Instant::now();
        let mut state = ear_state(PodSide::Right, 0x02, 0x00);
        state.report(report(0x00, 0x02), now);
        assert_eq!(state.in_ear(), Some((Some(true), Some(false))));
        state.set_primary(Some(PodSide::Left));
        assert_eq!(state.in_ear(), Some((Some(true), Some(false))));
    }

    /// With no primary pod packet after it, the report held is taken as it
    /// is once the hold is over: both pods may have been swapped by hand.
    #[test]
    fn a_held_report_is_taken_once_the_hold_is_over() {
        let start = Instant::now();
        let mut state = ear_state(PodSide::Left, 0x01, 0x00);
        state.report(report(0x00, 0x01), start);
        assert!(!state.release(start + ROLE_SWITCH_HOLD - ms(1)));
        assert_eq!(state.in_ear(), Some((Some(false), Some(true))));
        assert!(state.release(start + ROLE_SWITCH_HOLD));
        assert_eq!(state.in_ear(), Some((Some(true), Some(false))));
        assert_eq!(state.held_until(), None);
        assert!(!state.release(start + ROLE_SWITCH_HOLD * 2), "taken once");
    }

    #[test]
    fn the_hold_can_be_longer() {
        let start = Instant::now();
        let hold = Duration::from_secs(15);
        let mut state = EarState::with_hold(hold);
        state.set_primary(Some(PodSide::Left));
        state.report(report(0x01, 0x00), start);
        state.report(report(0x00, 0x01), start);
        assert_eq!(state.held_until(), Some(start + hold));
        assert!(!state.release(start + ROLE_SWITCH_HOLD));
        assert!(state.release(start + hold));
    }

    /// A report replaces one held: it is the newer news. It may be held in
    /// its turn, from its own time.
    #[test]
    fn a_report_replaces_one_held() {
        let start = Instant::now();
        let mut state = ear_state(PodSide::Left, 0x01, 0x00);
        state.report(report(0x00, 0x01), start);
        state.report(report(0x01, 0x01), start + ms(10));
        assert_eq!(state.in_ear(), Some((Some(false), Some(false))));
        assert_eq!(state.held_until(), None);

        let mut state = ear_state(PodSide::Left, 0x01, 0x00);
        state.report(report(0x00, 0x01), start);
        state.report(report(0x00, 0x02), start + ms(10));
        assert_eq!(state.held_until(), Some(start + ms(10) + ROLE_SWITCH_HOLD));
    }

    /// Only the swap is held: one pod in or out, both out, both in, and
    /// anything before the primary is known, are taken at once.
    #[test]
    fn only_a_swap_is_held() {
        let now = Instant::now();
        let taken = [
            ((0x00, 0x00), (0x00, 0x01)),
            ((0x00, 0x00), (0x01, 0x00)),
            ((0x01, 0x00), (0x00, 0x00)),
            ((0x01, 0x00), (0x01, 0x01)),
            ((0x00, 0x01), (0x01, 0x00)),
            ((0x01, 0x00), (0x00, 0x09)),
        ];
        for (last, next) in taken {
            let mut state = ear_state(PodSide::Left, last.0, last.1);
            state.report(report(next.0, next.1), now);
            assert_eq!(state.held_until(), None, "{last:02x?} to {next:02x?}");
        }

        let mut state = EarState::default();
        state.report(report(0x01, 0x00), now);
        state.report(report(0x00, 0x01), now);
        assert_eq!(state.held_until(), None, "no primary to read it with");
    }
}
