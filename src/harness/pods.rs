//! A pair of fake AirPods: the state of one pair, and the bytes it sends.
//!
//! Pure - state in, bytes out. There are no sockets, no D-Bus and no clock
//! here: time comes in as an argument, an offset from an epoch the caller
//! picks - the fake world's start, say - and must not run backwards. That
//! keeps every behavior testable without waiting, the timed ones included,
//! and leaves the I/O to the fake BlueZ and the world around it (#72).
//!
//! Everything sent goes through the builders beside the app's parsers -
//! `battery_packet`, `key_packet`, `Cleartext`, `Plaintext` and the rest - so
//! the fake cannot encode the protocol differently from how the app reads it.
//!
//! A pair talks on two channels, as real AirPods do:
//!
//! - **BLE.** [`Pods::advertisement`] is what the pair broadcasts at a moment:
//!   the Apple manufacturer data, from a resolvable private address that its
//!   IRK resolves, and that rotates.
//! - **AAP.** The world hands the model what the app sends
//!   ([`Pods::receive`]) and carries out whatever [`Pods::poll`] returns.
//!   Replies, notifications and the timed quirks all come out of `poll`.
//!
//! Where the hardware's behavior is not recorded, the fake makes a choice and
//! its documentation says so; the reference captures of #71 are what can
//! correct it.

use std::time::Duration;

use bluer::Address;

use crate::aap::battery::{Battery, Component, Status, battery_packet};
use crate::aap::ear::{EarReport, Place, ear_packet, primary_packet};
use crate::aap::keys::{KeyType, ProximityKey, key_packet};
use crate::aap::noise::{self, NoiseMode};
use crate::ble::decrypt::{KEY_LEN, Plaintext, ZEROED_SUFFIX, encrypt, random_key};
use crate::ble::parser::{Cleartext, PodSide, proximity_message};
use crate::ble::rpa::resolvable_address;

/// AirPods Pro 3, by the product ID `parse_proximity_data` reads.
pub const PRO_3: u16 = 0x2027;
/// AirPods Pro 2 with USB-C.
pub const PRO_2_USB_C: u16 = 0x2024;

/// How long the fake advertises from one address. Real AirPods advertise from
/// an address per pod, which in the #71 captures lasted 4 to 8 minutes,
/// connected or not, and changed across a closed lid as well. The fake
/// rotates far faster on purpose: it is there to put rotation through its
/// paces in tests.
pub const ADDRESS_ROTATION: Duration = Duration::from_secs(10);

/// How long a pair with the silent quirk holds a connection before hanging up,
/// as the one time it was seen.
pub const SILENT_HANG_UP: Duration = Duration::from_secs(9);

/// How long after a mode change its echo comes, by default: a Pro 3 echoed
/// 1.0-1.1 s after each change in the #71 captures. The real value, kept as
/// the default: the app records the mode it asks for at once and waits for
/// no echo, so nothing waits on it but a test that asks for it, which can
/// set [`Quirks::echo_delay`] shorter.
pub const ECHO_DELAY: Duration = Duration::from_secs(1);

/// How long after an Off its stale report of the mode it replaced comes, by
/// default: 0.33 s in the #71 capture - see [`Quirks::stale_report`].
pub const STALE_REPORT: Duration = Duration::from_millis(330);

/// Where an AAP packet carries its command.
const OPCODE: usize = 4;

/// How long after the ear report in the new order a role switch's first
/// primary pod packet comes. In the #142 captures the ear report came 1-31 ms
/// before it, at every switch.
pub const PRIMARY_AFTER_EAR: Duration = Duration::from_millis(10);

/// How long after a role switch's first primary pod packet its second one
/// comes: 30-90 ms in the #142 captures.
pub const SECOND_PRIMARY: Duration = Duration::from_millis(60);

// The requests a link sends as it comes up - `aap::Client::open`'s, and the key
// request `Coordinator::open_aap` adds - known by their command byte, as a
// device would know them: not by the packets beside the app's parsers, which a
// change on the wire would change here along with them.
const OP_HANDSHAKE: u8 = 0x01;
const OP_NOTIFICATIONS: u8 = 0x0F;
const OP_FEATURES: u8 = 0x4D;
const OP_KEY_REQUEST: u8 = 0x30;

/// The answer to the handshake. Pro 3 sends 18 bytes, starting
/// `01 00 04 00 00 00 01 00`; the app, like LibrePods, matches only these
/// four, so the fake sends just them.
const HANDSHAKE_ACK: [u8; 4] = [0x01, 0x00, 0x04, 0x00];

/// The answer to the features packet, as LibrePods takes it - by these six
/// bytes, and tested there with AirPods Pro 2 only. On AirPods Pro 3, 0x2B is
/// a list of known hosts that comes unasked with the handshake's answer, and
/// nothing answers the features packet (#71); #115 brings the fake in line.
const FEATURES_ACK: [u8; 6] = [0x04, 0x00, 0x04, 0x00, 0x2B, 0x00];

/// The startup dump's settings beside the mode and allow-Off, with the values
/// `docs/aap-noise-control.md` records - the ones it records none for are left
/// out. Several are valid mode bytes: 0x24's 03 reads as Transparency, 0x25's
/// 01 as Off. Taking a mode from any 0x09 packet without checking byte 6 reads
/// them as one.
const OTHER_SETTINGS: [(u8, &[u8]); 6] = [
    (0x1F, &[0x50, 0x50]),
    (0x24, &[0x03]),
    (0x25, &[0x01]),
    (0x26, &[0x01]),
    (0x28, &[0x01]),
    (0x2E, &[0x32]),
];

/// Byte 8 of the advertisement, connected to a host or not.
const CONNECTION_IDLE: u8 = 0x04;
const CONNECTION_NONE: u8 = 0x00;

/// A pair's proximity keys, what the key packet carries. Generated, never
/// written down: no real key may reach the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    /// Identity Resolving Key, type 0x01.
    pub irk: [u8; KEY_LEN],
    /// The key the advertisement is encrypted with, type 0x04.
    pub enc_key: [u8; KEY_LEN],
}

impl Keys {
    /// Fresh keys from `/dev/urandom`. The one place the model draws on
    /// randomness, and it does so only through [`Pods::new`] and
    /// [`Pods::repair`], which have pure counterparts taking the keys.
    pub fn generate() -> Self {
        Self {
            irk: random_key(),
            enc_key: random_key(),
        }
    }
}

/// A part of the pair that has a battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Left,
    Right,
    Case,
}

/// Behavior of the hardware that the fake can switch. The default is what
/// AirPods Pro 3 do in the #71 captures, for every model: AirPods Pro 2
/// (USB-C) count as behaving the same until a capture shows otherwise. Two
/// exceptions: [`Quirks::zero_suffix_when_connected`] zeroes the suffix while
/// connected, where a Pro 3 zeroes it whenever a pod is out of the case,
/// which #115 brings in line; and the address rotation, [`ADDRESS_ROTATION`],
/// is faster than the hardware's on purpose.
///
/// AirPods 4 answered a mode change otherwise (`docs/aap-noise-control.md`):
/// a 0x4B on every change, an echo on about one in four, and no report of the
/// mode an Off replaced. Their answers are `settings_changed: true,
/// echo_every: 4, stale_report: None`.
///
/// Each one is read when it applies, so a change takes effect from the next
/// packet, advertisement or connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirks {
    /// Ignore a bare Off until `0x34 01` has allowed it: no mode change and
    /// nothing on the wire, where recent firmware on Pro 3 and Pro 2 plays an
    /// error chime.
    pub off_needs_allow: bool,
    /// Answer every mode change with the 0x4B settings-changed notification,
    /// as AirPods 4 do. A Pro 3 sent none, so off by default.
    pub settings_changed: bool,
    /// Echo every n-th mode change with a 0x0D report of the mode set: 1
    /// echoes every change, 0 none. Counted rather than drawn at random, so
    /// tests are never flaky. The default 1 is a Pro 3's; AirPods 4 echoed
    /// about one change in four, which 4 gives, the first echo following the
    /// fourth change.
    pub echo_every: u32,
    /// How long after its change an echo comes: [`ECHO_DELAY`] by default, as
    /// a Pro 3 echoed 1.0-1.1 s after each change in the #71 captures. That
    /// puts it after the app has recorded the mode it asked for, so the echo
    /// confirms rather than sets it.
    pub echo_delay: Duration,
    /// Report the mode an Off replaced, this long after the change - before
    /// the echo, when one comes sooner than that. In the #71 capture, switched
    /// from Noise Cancellation to Off, a Pro 3 reported `0D 02` 0.33 s later
    /// and the Off itself, `0D 01`, at 1.4 s; the other three modes came with
    /// no such report. [`STALE_REPORT`] by default. It brings no echo of its
    /// own: [`Quirks::echo_every`] and [`Quirks::echo_delay`] say when that
    /// comes.
    pub stale_report: Option<Duration>,
    /// Zero the MAC suffix in the encrypted payload while connected to a host.
    /// Pro 3 zeroes it whenever a pod is out of the case instead, connected or
    /// not (#71); #115 brings the fake in line.
    pub zero_suffix_when_connected: bool,
    /// Accept an AAP connection, never answer, and hang up after
    /// [`SILENT_HANG_UP`] - seen once, with the Flatpak build. Off by default,
    /// and read when a connection opens. The app gives up on the handshake
    /// after 5 s and closes the connection first, so through the app the
    /// hang-up is never reached.
    pub silent_handshake: bool,
    /// Turn away this many AAP connections, from the next one on: each is
    /// taken and closed at once, unanswered. Counted down by each one turned
    /// away; 0, the default, takes every connection. How real AirPods refuse
    /// one is not recorded - a busy pair, a link not up yet. Through the app,
    /// a connection taken and closed is mostly a link that went during setup
    /// (#123): the handshake is sent and the close is read before an answer,
    /// which is logged at debug and tried again, never reaching the warnings
    /// of a refused connect. Closed before the app sends, the handshake
    /// fails to send, and that is a failed connect, with the warnings. Tried
    /// again either way, on a growing wait.
    pub refuse_aap: u32,
    /// How long after the notification request the battery and the startup
    /// dump come, holding the state as it was when asked. None by default; a
    /// pair seen in #62 took about a second, which the GNOME Settings battery
    /// went missing for.
    pub report_delay: Duration,
}

impl Default for Quirks {
    fn default() -> Self {
        Self {
            off_needs_allow: true,
            settings_changed: false,
            echo_every: 1,
            echo_delay: ECHO_DELAY,
            stale_report: Some(STALE_REPORT),
            zero_suffix_when_connected: true,
            silent_handshake: false,
            refuse_aap: 0,
            report_delay: Duration::ZERO,
        }
    }
}

/// One advertisement, as BlueZ hands it to the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    /// The random address it comes from: a `Device1` of its own, not the pair's
    /// real address. A resolvable private address, made from the pair's IRK.
    pub address: Address,
    /// The value of the Apple (0x004C) entry in that device's
    /// `ManufacturerData` - the message type, length and payload that
    /// `parse_proximity_data` takes.
    pub data: Vec<u8>,
}

/// What to do on the pair's AAP connection, as [`Pods::poll`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this packet to the app.
    Send(Vec<u8>),
    /// Close the connection. Nothing follows it.
    HangUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pod {
    battery: Option<u8>,
    charging: bool,
    in_ear: bool,
    in_case: bool,
}

/// One AAP connection, from the app's connect to either side's hang-up.
#[derive(Debug, Clone)]
struct Session {
    /// Fixed when the connection opens: a quirk switched later does not turn a
    /// connection silent halfway.
    silent: bool,
    /// What the app was last told, from the moment it subscribed; `None`
    /// before. A change from it is what `poll` reports.
    reported: Option<Reported>,
    /// Where the pods are and which is primary, as the app was last told,
    /// from the handshake's answer on; `None` before. These come unasked.
    ears_reported: Option<Ears>,
    /// Actions waiting for their time, in time order.
    scheduled: Vec<(Duration, Action)>,
}

impl Session {
    /// Queues `action` for `at`, after anything already queued for that time.
    fn schedule(&mut self, at: Duration, action: Action) {
        let i = self.scheduled.partition_point(|(t, _)| *t <= at);
        self.scheduled.insert(i, (at, action));
    }
}

/// What the ear report and the primary pod packet carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ears {
    primary: PodSide,
    report: EarReport,
}

impl Ears {
    /// The primary pod packet's `SS`: whether the secondary pod can be
    /// reached, as it cannot from the closed case.
    fn secondary_reachable(self) -> bool {
        self.report.secondary != Place::InClosedCase
    }

    fn primary_packet(self, switching: bool) -> Vec<u8> {
        primary_packet(self.primary, self.secondary_reachable(), switching).to_vec()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reported {
    batteries: [Battery; 3],
    mode: NoiseMode,
}

/// A pair of fake AirPods.
///
/// Setters change the state and nothing else. The advertisement reflects them
/// from the next call; over AAP, [`Pods::poll`] reports what changed.
#[derive(Debug, Clone)]
pub struct Pods {
    mac: Address,
    product_id: u16,
    color: u8,
    left: Pod,
    right: Pod,
    case_battery: Option<u8>,
    case_charging: bool,
    lid_open: bool,
    primary: PodSide,
    noise_mode: NoiseMode,
    allow_off: bool,
    keys: Keys,
    connected: bool,
    session: Option<Session>,
    /// Mode changes taken over AAP, for [`Quirks::echo_every`].
    mode_changes: u32,
    received: Vec<Vec<u8>>,
    pub quirks: Quirks,
}

impl Pods {
    /// A pair at the real address `mac`, with fresh keys.
    ///
    /// It starts as if just taken out of the box: both pods in the open case,
    /// left at 80%, right at 70% and the case at 50% - different, so a
    /// left/right mix-up shows - nothing charging, white, left primary, noise
    /// cancellation, Off not allowed, and connected to nothing.
    pub fn new(product_id: u16, mac: Address) -> Self {
        Self::with_keys(product_id, mac, Keys::generate())
    }

    /// [`Pods::new`] with the keys given.
    pub fn with_keys(product_id: u16, mac: Address, keys: Keys) -> Self {
        let pod = |battery| Pod {
            battery: Some(battery),
            charging: false,
            in_ear: false,
            in_case: true,
        };
        Self {
            mac,
            product_id,
            color: 0x00,
            left: pod(80),
            right: pod(70),
            case_battery: Some(50),
            case_charging: false,
            lid_open: true,
            primary: PodSide::Left,
            noise_mode: NoiseMode::NoiseCancellation,
            allow_off: false,
            keys,
            connected: false,
            session: None,
            mode_changes: 0,
            received: Vec::new(),
            quirks: Quirks::default(),
        }
    }

    /// The real address: the one BlueZ pairs with and AAP connects to.
    pub fn mac(&self) -> Address {
        self.mac
    }

    pub fn product_id(&self) -> u16 {
        self.product_id
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    pub fn battery(&self, part: Part) -> Option<u8> {
        match part {
            Part::Left => self.left.battery,
            Part::Right => self.right.battery,
            Part::Case => self.case_battery,
        }
    }

    pub fn noise_mode(&self) -> NoiseMode {
        self.noise_mode
    }

    /// Whether Off is allowed as a listening mode, the setting `0x34` writes.
    pub fn allow_off(&self) -> bool {
        self.allow_off
    }

    /// Whether the pair is connected to a host - this one or another.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Whether the app has an AAP connection open.
    pub fn has_aap(&self) -> bool {
        self.session.is_some()
    }

    /// Every packet the app has sent, in order, across connections.
    pub fn received(&self) -> &[Vec<u8>] {
        &self.received
    }

    /// A battery level in percent; `None` for unknown.
    ///
    /// # Panics
    ///
    /// With a level above 100.
    pub fn set_battery(&mut self, part: Part, level: Option<u8>) {
        assert!(
            level.is_none_or(|l| l <= 100),
            "a battery level is 0-100%, got {level:?}"
        );
        match part {
            Part::Left => self.left.battery = level,
            Part::Right => self.right.battery = level,
            Part::Case => self.case_battery = level,
        }
    }

    pub fn set_charging(&mut self, part: Part, charging: bool) {
        match part {
            Part::Left => self.left.charging = charging,
            Part::Right => self.right.charging = charging,
            Part::Case => self.case_charging = charging,
        }
    }

    /// Puts a pod in an ear, or takes it out. A pod in an ear is out of the
    /// case. Over AAP, the next [`Pods::poll`] sends an ear report.
    pub fn set_in_ear(&mut self, side: PodSide, in_ear: bool) {
        let pod = self.pod_mut(side);
        pod.in_ear = in_ear;
        pod.in_case &= !in_ear;
    }

    /// Puts a pod in the case, or takes it out. A pod in the case is out of
    /// the ear. Over AAP, the next [`Pods::poll`] sends an ear report.
    pub fn set_in_case(&mut self, side: PodSide, in_case: bool) {
        let pod = self.pod_mut(side);
        pod.in_case = in_case;
        pod.in_ear &= !in_case;
    }

    /// Opens or closes the case. The advertisement carries the lid only while
    /// the pod that sends it is in the case, or both are. With the sending pod
    /// worn and the other in the case, it says only that a pod is in the case,
    /// and the app keeps the lid it last heard. Over AAP, a pod in the case
    /// is in the closed case at once - see [`Pods::poll`].
    pub fn set_lid(&mut self, open: bool) {
        self.lid_open = open;
    }

    /// Makes a pod primary, as the pair does at a role switch. Over AAP, the
    /// next [`Pods::poll`] sends the ear report in the new order, and the
    /// primary pod packets after it - see [`PRIMARY_AFTER_EAR`].
    pub fn set_primary(&mut self, side: PodSide) {
        self.primary = side;
    }

    pub fn set_color(&mut self, color: u8) {
        self.color = color;
    }

    /// Connects the pair to a host, or disconnects it - this one or another,
    /// such as a phone. A connected fake zeroes the suffix in its advertisement
    /// (see [`Quirks::zero_suffix_when_connected`]) and reports itself idle in
    /// byte 8. Disconnecting takes an open AAP connection with it: the next
    /// [`Pods::poll`] hangs up.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Changes the noise control mode from elsewhere - a stem press, or a
    /// phone. Over AAP, the next [`Pods::poll`] reports it.
    pub fn set_noise_mode(&mut self, mode: NoiseMode) {
        self.noise_mode = mode;
    }

    /// Re-pairs, which gives the pair new keys. Connections are left to the
    /// caller: a key request answered after this carries the new keys.
    pub fn repair(&mut self) {
        self.repair_with(Keys::generate());
    }

    /// [`Pods::repair`] with the keys given.
    pub fn repair_with(&mut self, keys: Keys) {
        self.keys = keys;
    }

    /// What the pair broadcasts at `now`.
    ///
    /// The cleartext gives the batteries in 10% steps. The encrypted portion,
    /// under the pair's ENC_KEY, gives them exact, with the real address's last
    /// three bytes in bytes 7-9 - zeroed while connected, see
    /// [`Quirks::zero_suffix_when_connected`] - and a tail that
    /// changes with `now`.
    pub fn advertisement(&self, now: Duration) -> Advertisement {
        let (primary, secondary) = match self.primary {
            PodSide::Left => (&self.left, &self.right),
            PodSide::Right => (&self.right, &self.left),
        };
        // The primary pod is the one that does the talking.
        let this_in_case = primary.in_case;
        let both_in_case = self.left.in_case && self.right.in_case;
        let one_in_case = self.left.in_case || self.right.in_case;
        let cleartext = Cleartext {
            product_id: self.product_id,
            primary: self.primary,
            this_in_case,
            both_in_case,
            one_in_case,
            left_battery: self.left.battery,
            right_battery: self.right.battery,
            case_battery: self.case_battery,
            left_charging: self.left.charging,
            right_charging: self.right.charging,
            case_charging: self.case_charging,
            left_in_ear: self.left.in_ear,
            right_in_ear: self.right.in_ear,
            // With one pod in the case and one worn, the #71 captures have the
            // pod in the case report the lid and the worn one only status bit 4.
            // Only the primary pod talks here, so a worn primary leaves the
            // app the lid it last heard.
            lid_open: (this_in_case || both_in_case).then_some(self.lid_open),
            color: self.color,
            connection_state: if self.connected {
                CONNECTION_IDLE
            } else {
                CONNECTION_NONE
            },
        };

        let mac_suffix = if self.connected && self.quirks.zero_suffix_when_connected {
            ZEROED_SUFFIX
        } else {
            [self.mac[3], self.mac[4], self.mac[5]]
        };
        let tail = self.derive(b'T', now.as_nanos() as u64);
        let plaintext = Plaintext {
            header: self.plaintext_header(),
            primary_battery: primary.battery,
            primary_charging: primary.charging,
            secondary_battery: secondary.battery,
            secondary_charging: secondary.charging,
            case_battery: self.case_battery,
            case_charging: self.case_charging,
            mac_suffix,
            tail: [tail[0], tail[1], tail[2], tail[3]],
        };
        let encrypted = encrypt(&plaintext.to_bytes(), &self.keys.enc_key);

        Advertisement {
            address: self.address_at(now),
            data: proximity_message(&cleartext.to_bytes(), &encrypted),
        }
    }

    /// The random address the pair advertises from at `now`. It changes every
    /// [`ADDRESS_ROTATION`], and with the keys.
    ///
    /// A resolvable private address, as real AirPods advertise from: its hash
    /// is `ah` of its `prand` under the pair's IRK, so the IRK the key packet
    /// carries resolves it as it resolves the hardware's (#134).
    pub fn address_at(&self, now: Duration) -> Address {
        let period = (now.as_millis() / ADDRESS_ROTATION.as_millis()) as u64;
        let bytes = self.derive(b'A', period);
        resolvable_address(&self.keys.irk, [bytes[0], bytes[1], bytes[2]])
    }

    /// The app opened an AAP connection at `now`. There is no AAP without a
    /// Bluetooth link, so the pair is connected from here on.
    pub fn aap_opened(&mut self, now: Duration) {
        self.connected = true;
        let mut session = Session {
            silent: self.quirks.silent_handshake,
            reported: None,
            ears_reported: None,
            scheduled: Vec::new(),
        };
        if session.silent {
            session.schedule(now + SILENT_HANG_UP, Action::HangUp);
        }
        self.session = Some(session);
    }

    /// Whether the pair turns away an AAP connection the app is opening - see
    /// [`Quirks::refuse_aap`] - counting it off the quirk if so. One turned
    /// away opens nothing: [`Pods::aap_opened`] is for those taken.
    pub fn refuses_aap(&mut self) -> bool {
        if self.quirks.refuse_aap == 0 {
            return false;
        }
        self.quirks.refuse_aap -= 1;
        true
    }

    /// The app closed the AAP connection, or it dropped. Whatever was still
    /// due on it goes with it.
    pub fn aap_closed(&mut self) {
        self.session = None;
    }

    /// A packet the app sent at `now`. Whatever it calls for comes out of the
    /// next [`Pods::poll`].
    ///
    /// Requests are known by their command byte, and settings by their
    /// sub-command too:
    ///
    /// - the handshake gets its acknowledgment, then the primary pod packet
    ///   (0x08) and the ear report (0x06), unasked, and from then on a report
    ///   of every change to them;
    /// - the notification request (0x0F) the battery and the startup dump - the
    ///   mode, allow-Off and other 0x09 settings - and from then on a report of
    ///   every change;
    /// - the features packet (0x4D) the 0x2B LibrePods takes for its
    ///   acknowledgment - see `FEATURES_ACK`;
    /// - the key request (0x30) the key packet, IRK then ENC_KEY;
    /// - a mode change its delayed echo, and an Off a report of the mode it
    ///   replaced ahead of that, as a Pro 3 answers; or nothing, for an Off not
    ///   allowed - see [`Quirks`], which switch to AirPods 4's answers too;
    /// - `0x34` sets allow-Off. What it answers is not recorded, so the fake
    ///   answers nothing.
    ///
    /// Anything else is recorded and otherwise ignored, as is everything
    /// arriving with no connection open or on a silent one.
    pub fn receive(&mut self, packet: &[u8], now: Duration) {
        self.received.push(packet.to_vec());
        if !self.session.as_ref().is_some_and(|s| !s.silent) {
            return;
        }
        match packet.get(OPCODE).copied() {
            Some(OP_HANDSHAKE) => self.answer_handshake(now),
            Some(OP_FEATURES) => self.send(now, FEATURES_ACK.to_vec()),
            Some(OP_NOTIFICATIONS) => self.start_notifications(now),
            Some(OP_KEY_REQUEST) => self.send(now, self.key_packet()),
            _ => self.settings_command(packet, now),
        }
    }

    /// What is due on the AAP connection by `now`, in order: replies and timed
    /// actions as they fall due, then a report of each change the app has not
    /// been told of - batteries and charging in one battery packet, the mode in
    /// a 0x0D report, the pods' places in an ear report. Taken once; a second
    /// call at the same time returns nothing new.
    ///
    /// A new primary pod comes as the hardware's does: the ear report in the
    /// new order now, then two primary pod packets, the first
    /// [`PRIMARY_AFTER_EAR`] later and marked as a switch, the second
    /// [`SECOND_PRIMARY`] after that. A pod in the case is in the closed case
    /// the moment the lid closes, where the hardware took about 4 s, and
    /// then the primary pod packet says the secondary cannot be reached.
    ///
    /// A [`Action::HangUp`] ends the connection, and nothing comes after it.
    pub fn poll(&mut self, now: Duration) -> Vec<Action> {
        let batteries = self.aap_batteries();
        let mode = self.noise_mode;
        let ears = self.ears();
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        if !self.connected {
            // The link went down under the connection, and takes it along.
            self.session = None;
            return vec![Action::HangUp];
        }

        let due = session.scheduled.partition_point(|(at, _)| *at <= now);
        let mut actions: Vec<Action> = session.scheduled.drain(..due).map(|(_, a)| a).collect();
        if let Some(i) = actions.iter().position(|a| *a == Action::HangUp) {
            actions.truncate(i + 1);
            self.session = None;
            return actions;
        }

        if let Some(reported) = &mut session.reported {
            if reported.batteries != batteries {
                actions.push(Action::Send(battery_packet(&batteries)));
                reported.batteries = batteries;
            }
            if reported.mode != mode {
                actions.push(Action::Send(noise::set_packet(mode).to_vec()));
                reported.mode = mode;
            }
        }
        if let Some(reported) = &mut session.ears_reported
            && *reported != ears
        {
            let reported = std::mem::replace(reported, ears);
            if reported.report != ears.report {
                actions.push(Action::Send(ear_packet(ears.report).to_vec()));
            }
            if reported.primary != ears.primary {
                let first = now + PRIMARY_AFTER_EAR;
                session.schedule(first, Action::Send(ears.primary_packet(true)));
                session.schedule(
                    first + SECOND_PRIMARY,
                    Action::Send(ears.primary_packet(false)),
                );
            } else if reported.secondary_reachable() != ears.secondary_reachable() {
                actions.push(Action::Send(ears.primary_packet(false)));
            }
        }
        actions
    }

    /// When the next timed action falls due - a delayed echo, or the silent
    /// pair's hang-up - for the caller to [`Pods::poll`] at. `None` with
    /// nothing scheduled. Replies and changes made through the setters are due
    /// at once and not counted here: poll after each.
    pub fn next_due(&self) -> Option<Duration> {
        self.session.as_ref()?.scheduled.first().map(|(at, _)| *at)
    }

    /// Where a pod is, as an ear report gives it.
    fn place(&self, pod: &Pod) -> Place {
        match (pod.in_ear, pod.in_case, self.lid_open) {
            (true, _, _) => Place::InEar,
            (false, true, true) => Place::InCase,
            (false, true, false) => Place::InClosedCase,
            (false, false, _) => Place::Out,
        }
    }

    /// The primary pod, and the ear report in its order.
    fn ears(&self) -> Ears {
        let (primary, secondary) = match self.primary {
            PodSide::Left => (&self.left, &self.right),
            PodSide::Right => (&self.right, &self.left),
        };
        Ears {
            primary: self.primary,
            report: EarReport {
                primary: self.place(primary),
                secondary: self.place(secondary),
            },
        }
    }

    /// The handshake's answer, and what comes unasked right after it: the
    /// primary pod, then the ear report, as the #142 captures have them.
    fn answer_handshake(&mut self, now: Duration) {
        let ears = self.ears();
        self.send(now, HANDSHAKE_ACK.to_vec());
        self.send(now, ears.primary_packet(false));
        self.send(now, ear_packet(ears.report).to_vec());
        if let Some(session) = &mut self.session {
            session.ears_reported = Some(ears);
        }
    }

    fn pod_mut(&mut self, side: PodSide) -> &mut Pod {
        match side {
            PodSide::Left => &mut self.left,
            PodSide::Right => &mut self.right,
        }
    }

    /// Byte 0 of the plaintext: 0x10 as Pro 3 sends it, 0x00 as Pro 2 does -
    /// and as the fake sends it for every other model. Its meaning is unknown,
    /// and nothing reads it.
    fn plaintext_header(&self) -> u8 {
        if self.product_id == PRO_3 { 0x10 } else { 0x00 }
    }

    /// Sixteen bytes that look random but follow from the IRK, a purpose and a
    /// number, so the rotating address and the advertisement's changing tail
    /// are a function of state and time like everything else here.
    fn derive(&self, purpose: u8, n: u64) -> [u8; 16] {
        let mut block = [0u8; 16];
        block[0] = purpose;
        block[8..].copy_from_slice(&n.to_be_bytes());
        encrypt(&block, &self.keys.irk)
    }

    /// The batteries as AAP reports them, the primary pod first, then the
    /// other, then the case.
    ///
    /// The hardware lists first the bud AAP reached, itself (#71), and in the
    /// #142 captures that was the primary pod: left first with `08 01`, right
    /// first with `08 02`. The fake takes the primary, so the order follows a
    /// role switch.
    ///
    /// A part that is away is sent as the hardware sends it: status
    /// Disconnected (`0x04`) at level 0, the level that once read as a real
    /// 0% (#108). Away is a level the model holds as unknown, and a pod in the
    /// closed case, which takes the case's battery with it (capture 4 of #71).
    fn aap_batteries(&self) -> [Battery; 3] {
        let closed = |pod: &Pod| pod.in_case && !self.lid_open;
        let battery = |component, level: Option<u8>, charging, away: bool| match level {
            Some(level) if !away => Battery {
                component,
                level: Some(level),
                status: if charging {
                    Status::Charging
                } else {
                    Status::Discharging
                },
            },
            _ => Battery {
                component,
                level: Some(0),
                status: Status::Disconnected,
            },
        };
        let left = battery(
            Component::Left,
            self.left.battery,
            self.left.charging,
            closed(&self.left),
        );
        let right = battery(
            Component::Right,
            self.right.battery,
            self.right.charging,
            closed(&self.right),
        );
        let case = battery(
            Component::Case,
            self.case_battery,
            self.case_charging,
            closed(&self.left) || closed(&self.right),
        );
        match self.primary {
            PodSide::Left => [left, right, case],
            PodSide::Right => [right, left, case],
        }
    }

    fn key_packet(&self) -> Vec<u8> {
        key_packet(&[
            ProximityKey {
                key_type: KeyType::Irk,
                data: self.keys.irk.to_vec(),
            },
            ProximityKey {
                key_type: KeyType::EncKey,
                data: self.keys.enc_key.to_vec(),
            },
        ])
    }

    /// Queues `packet` on the open connection for `at`.
    fn send(&mut self, at: Duration, packet: Vec<u8>) {
        if let Some(session) = &mut self.session {
            session.schedule(at, Action::Send(packet));
        }
    }

    /// The answer to the notification request: the battery, then the startup
    /// dump. From here on the app hears of every change.
    fn start_notifications(&mut self, now: Duration) {
        let batteries = self.aap_batteries();
        let mut packets = vec![
            battery_packet(&batteries),
            // The mode packet serves both directions: this is the report.
            noise::set_packet(self.noise_mode).to_vec(),
            noise::allow_off_packet(self.allow_off).to_vec(),
        ];
        packets.extend(
            OTHER_SETTINGS
                .iter()
                .map(|(sub_cmd, value)| noise::settings_report(*sub_cmd, value).to_vec()),
        );
        let at = now + self.quirks.report_delay;
        if let Some(session) = &mut self.session {
            for packet in packets {
                session.schedule(at, Action::Send(packet));
            }
            session.reported = Some(Reported {
                batteries,
                mode: self.noise_mode,
            });
        }
    }

    /// A command in the 0x09 settings family.
    fn settings_command(&mut self, packet: &[u8], now: Duration) {
        if let Some(allowed) = noise::parse_allow_off_packet(packet) {
            self.allow_off = allowed;
            return;
        }
        let Ok(mode) = noise::parse_noise_mode_packet(packet) else {
            return;
        };
        if mode == NoiseMode::Off && !self.allow_off && self.quirks.off_needs_allow {
            // The error chime: the mode stays, and nothing is sent.
            return;
        }

        let previous = std::mem::replace(&mut self.noise_mode, mode);
        self.mode_changes += 1;
        let Some(session) = &mut self.session else {
            return;
        };
        // The app knows what it asked for; only a change from elsewhere is
        // news to it.
        if let Some(reported) = &mut session.reported {
            reported.mode = mode;
        }
        if self.quirks.settings_changed {
            session.schedule(now, Action::Send(noise::settings_changed_packet().to_vec()));
        }
        // Stale, so not what the app was told: `reported` keeps the mode set,
        // and only the echo corrects the report.
        if let Some(delay) = self.quirks.stale_report
            && mode == NoiseMode::Off
            && previous != NoiseMode::Off
        {
            session.schedule(
                now + delay,
                Action::Send(noise::set_packet(previous).to_vec()),
            );
        }
        // Carries the mode this command set, even if another has replaced it
        // by the time it arrives: a stale report, like those in run 1 of
        // docs/aap-noise-control.md.
        if self.quirks.echo_every != 0 && self.mode_changes.is_multiple_of(self.quirks.echo_every) {
            session.schedule(
                now + self.quirks.echo_delay,
                Action::Send(noise::set_packet(mode).to_vec()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::{
        BatteryInfo, find_encryption_key, is_noise_mode_packet, parse_battery_packet,
        parse_noise_mode_packet, parse_proximity_keys,
    };
    use crate::ble::decrypt::{decrypt_for_device, decrypt_raw, matches_device};
    use crate::ble::parser::{ProximityData, parse_proximity_data};
    use crate::ble::rpa::resolves;

    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    // What `aap::client` sends, spelled out so a change on the wire shows here.
    const HANDSHAKE: [u8; 16] = [
        0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    const NOTIFICATIONS: [u8; 10] = [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
    const FEATURES: [u8; 14] = [
        0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const KEY_REQUEST: [u8; 8] = [0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00];

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// The advertisement at `now`, parsed and decrypted as the app does it.
    fn read(pods: &Pods, now: Duration) -> ProximityData {
        let ad = pods.advertisement(now);
        let mut pd = parse_proximity_data(&ad.data).unwrap();
        let encrypted = pd.encrypted_portion().unwrap().to_vec();
        let decrypted = decrypt_for_device(&encrypted, &pods.keys().enc_key, pods.mac()).unwrap();
        pd.add_decrypted_data(&decrypted).unwrap();
        pd
    }

    /// The plaintext of the advertisement at `now`, unchecked.
    fn plaintext(pods: &Pods, now: Duration) -> [u8; 16] {
        let pd = parse_proximity_data(&pods.advertisement(now).data).unwrap();
        decrypt_raw(pd.encrypted_portion().unwrap(), &pods.keys().enc_key).unwrap()
    }

    /// The packets among `actions`.
    fn sent(actions: &[Action]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .map(|a| match a {
                Action::Send(p) => p.clone(),
                Action::HangUp => panic!("hung up among {actions:?}"),
            })
            .collect()
    }

    /// A Pro 3 pair with an AAP connection the app has run its connect
    /// sequence on, the replies taken.
    fn subscribed() -> Pods {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.aap_opened(ms(0));
        for packet in [&HANDSHAKE[..], &NOTIFICATIONS, &FEATURES, &KEY_REQUEST] {
            pods.receive(packet, ms(0));
        }
        pods.poll(ms(0));
        pods
    }

    #[test]
    fn advertises_its_state() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_color(0x01);
        pods.set_in_ear(PodSide::Left, true);
        pods.set_battery(Part::Left, Some(83));
        pods.set_charging(Part::Left, true);
        pods.set_battery(Part::Right, Some(41));
        pods.set_in_case(PodSide::Right, false);
        pods.set_battery(Part::Case, None);

        let ad = pods.advertisement(ms(0));
        let clear = parse_proximity_data(&ad.data).unwrap();
        assert_eq!(clear.product_id, PRO_3);
        assert_eq!(clear.color, 0x01);
        assert_eq!(
            (clear.left_battery, clear.right_battery),
            (Some(80), Some(40))
        );
        assert_eq!(clear.case_battery, None);
        assert!(clear.left_charging && !clear.right_charging);
        assert!(clear.left_in_ear && !clear.right_in_ear);
        assert_eq!(clear.lid_open, None, "both pods are out of the case");
        assert_eq!(clear.connection_state, CONNECTION_NONE);

        let exact = read(&pods, ms(0));
        assert_eq!(
            (exact.left_battery, exact.right_battery),
            (Some(83), Some(41))
        );
        assert_eq!(exact.case_battery, None);
        assert!(exact.left_charging);
    }

    #[test]
    fn advertises_the_primary_pod() {
        let mut pods = Pods::new(PRO_2_USB_C, MAC);
        pods.set_primary(PodSide::Right);
        pods.set_in_ear(PodSide::Right, true);
        pods.set_battery(Part::Right, Some(12));
        pods.set_charging(Part::Right, true);

        let pd = read(&pods, ms(0));
        assert!(pd.is_flipped);
        assert_eq!(pd.right_battery, Some(12));
        assert!(pd.right_charging);
        assert_eq!(pd.left_battery, Some(80));
        assert!(pd.right_in_ear && !pd.left_in_ear);
        assert_eq!(pd.lid_open, None, "the right pod, worn, does the talking");
        assert!(pd.one_in_case, "and says the left pod is in the case");
    }

    #[test]
    fn advertises_the_lid_while_the_sending_pod_is_in_the_case() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_lid(false);
        assert_eq!(read(&pods, ms(0)).lid_open, Some(false));
        pods.set_lid(true);
        assert_eq!(read(&pods, ms(0)).lid_open, Some(true));
        pods.set_in_ear(PodSide::Right, true);
        assert_eq!(
            read(&pods, ms(0)).lid_open,
            Some(true),
            "the left pod, in the case, does the talking"
        );
        pods.set_in_ear(PodSide::Left, true);
        assert_eq!(read(&pods, ms(0)).lid_open, None);
    }

    /// Disconnected, bytes 7-9 carry the real address's suffix; connected,
    /// zeros. Either way the pair's key decrypts it and `matches_device`
    /// takes it.
    #[test]
    fn decrypts_with_its_key_connected_or_not() {
        let mut pods = Pods::new(PRO_3, MAC);
        let plain = plaintext(&pods, ms(0));
        assert_eq!(plain[7..10], [0xDD, 0xEE, 0xFF]);
        assert!(matches_device(&plain, MAC));

        pods.set_connected(true);
        let plain = plaintext(&pods, ms(0));
        assert_eq!(plain[7..10], ZEROED_SUFFIX);
        assert!(matches_device(&plain, MAC));
        assert_eq!(read(&pods, ms(0)).connection_state, CONNECTION_IDLE);
    }

    #[test]
    fn the_zeroed_suffix_is_a_quirk() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.zero_suffix_when_connected = false;
        pods.set_connected(true);
        assert_eq!(plaintext(&pods, ms(0))[7..10], [0xDD, 0xEE, 0xFF]);
    }

    /// A wrong key passes by chance one time in 2^23 - the odds the app works
    /// with too, and the only way this test can fail spuriously.
    #[test]
    fn a_different_key_fails() {
        for connected in [false, true] {
            let mut pods = Pods::new(PRO_3, MAC);
            pods.set_connected(connected);
            let pd = parse_proximity_data(&pods.advertisement(ms(0)).data).unwrap();
            let encrypted = pd.encrypted_portion().unwrap();
            let other = Keys::generate();
            assert!(decrypt_for_device(encrypted, &other.enc_key, MAC).is_err());
            // The IRK is a key of the pair's, but not this one.
            assert!(decrypt_for_device(encrypted, &pods.keys().irk, MAC).is_err());
        }
    }

    /// Every address resolves with the IRK the key packet carries, as the
    /// hardware's do, and with no other pair's.
    #[test]
    fn the_address_rotates_and_resolves_with_the_irk() {
        let pods = Pods::new(PRO_3, MAC);
        let first = pods.address_at(ms(0));
        assert_eq!(pods.address_at(ADDRESS_ROTATION - ms(1)), first);
        let second = pods.address_at(ADDRESS_ROTATION);
        assert_ne!(second, first);
        assert_eq!(pods.advertisement(ADDRESS_ROTATION).address, second);
        let other = Pods::new(PRO_3, MAC);
        for address in [first, second] {
            assert_ne!(address, MAC);
            assert!(resolves(&pods.keys().irk, address), "{address}");
            assert!(!resolves(&other.keys().irk, address), "{address}");
        }
        assert_ne!(other.address_at(ms(0)), first, "every pair has its own");
    }

    /// Bytes 12-15 of the plaintext change from one advertisement to the
    /// next, and the ciphertext with them.
    #[test]
    fn every_advertisement_differs() {
        let pods = Pods::new(PRO_3, MAC);
        assert_ne!(plaintext(&pods, ms(0))[12..], plaintext(&pods, ms(1))[12..]);
        assert_eq!(plaintext(&pods, ms(0))[..12], plaintext(&pods, ms(1))[..12]);
        assert_eq!(pods.advertisement(ms(0)), pods.advertisement(ms(0)));
    }

    #[test]
    fn repairing_changes_the_keys_and_the_address() {
        let mut pods = Pods::new(PRO_3, MAC);
        let keys = pods.keys().clone();
        let address = pods.address_at(ms(0));
        pods.repair();
        assert_ne!(pods.keys().enc_key, keys.enc_key);
        assert_ne!(pods.keys().irk, keys.irk);
        assert_ne!(pods.address_at(ms(0)), address);
        assert!(!resolves(&keys.irk, pods.address_at(ms(0))));
        read(&pods, ms(0));

        let fixed = Keys::generate();
        pods.repair_with(fixed.clone());
        assert_eq!(pods.keys(), &fixed);
    }

    #[test]
    fn answers_the_connect_sequence() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_battery(Part::Left, Some(83));
        pods.set_charging(Part::Case, true);
        pods.aap_opened(ms(0));
        assert!(pods.is_connected(), "AAP needs a Bluetooth link");

        pods.receive(&HANDSHAKE, ms(0));
        assert_eq!(sent(&pods.poll(ms(0))), handshake_answer(&pods));

        pods.receive(&NOTIFICATIONS, ms(0));
        let dump = sent(&pods.poll(ms(0)));
        let info = parse_battery_packet(&dump[0]).unwrap();
        assert_eq!(info.left.unwrap().level, Some(83));
        assert_eq!(info.right.unwrap().level, Some(70));
        assert!(info.case.unwrap().is_charging());
        assert_eq!(dump.len(), 3 + OTHER_SETTINGS.len());

        pods.receive(&FEATURES, ms(0));
        assert_eq!(sent(&pods.poll(ms(0))), [FEATURES_ACK.to_vec()]);

        pods.receive(&KEY_REQUEST, ms(0));
        let reply = sent(&pods.poll(ms(0)));
        let keys = parse_proximity_keys(&reply[0]).unwrap();
        assert_eq!(find_encryption_key(&keys), Ok(pods.keys().enc_key));
        assert_eq!(keys[0].key_type, KeyType::Irk);
        assert_eq!(keys[0].data, pods.keys().irk);

        assert!(pods.poll(ms(1000)).is_empty());
    }

    /// The mode is one 0x09 setting among several, some of them with values
    /// that read as a mode. Only byte 6 picks it out.
    #[test]
    fn the_startup_dump_hides_the_mode_among_other_settings() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_noise_mode(NoiseMode::Adaptive);
        pods.aap_opened(ms(0));
        pods.receive(&NOTIFICATIONS, ms(0));
        let settings: Vec<Vec<u8>> = sent(&pods.poll(ms(0)))
            .into_iter()
            .filter(|p| p[OPCODE] == 0x09)
            .collect();

        let modes: Vec<&Vec<u8>> = settings
            .iter()
            .filter(|p| is_noise_mode_packet(p))
            .collect();
        assert_eq!(modes.len(), 1);
        assert_eq!(parse_noise_mode_packet(modes[0]), Ok(NoiseMode::Adaptive));
        let lookalikes = settings
            .iter()
            .filter(|p| !is_noise_mode_packet(p) && NoiseMode::from_byte(p[7]).is_some())
            .count();
        assert!(lookalikes >= 2, "{settings:02x?}");
        assert!(settings.contains(&noise::allow_off_packet(false).to_vec()));
    }

    #[test]
    fn a_bare_off_is_ignored_until_allowed() {
        let mut pods = subscribed();
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(10));
        assert!(pods.poll(ms(10)).is_empty());
        assert_eq!(pods.noise_mode(), NoiseMode::NoiseCancellation);

        pods.receive(&noise::allow_off_packet(true), ms(20));
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(20));
        assert!(pods.allow_off());
        assert_eq!(pods.noise_mode(), NoiseMode::Off);
        assert!(pods.poll(ms(20)).is_empty(), "nothing at once");
        assert_eq!(pods.next_due(), Some(ms(20) + STALE_REPORT));
    }

    #[test]
    fn off_needs_no_allowing_without_the_quirk() {
        let mut pods = subscribed();
        pods.quirks.off_needs_allow = false;
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(10));
        assert_eq!(pods.noise_mode(), NoiseMode::Off);
    }

    /// A Pro 3's answers, the default: nothing at once, and the echo of every
    /// change [`ECHO_DELAY`] later, carrying the mode.
    #[test]
    fn answers_mode_changes_as_a_pro_3_does() {
        let mut pods = subscribed();
        let modes = [
            NoiseMode::Transparency,
            NoiseMode::Adaptive,
            NoiseMode::NoiseCancellation,
        ];
        for (i, mode) in modes.into_iter().enumerate() {
            let now = ms(2000 * (i as u64 + 1));
            pods.receive(&noise::set_packet(mode), now);
            assert_eq!(pods.noise_mode(), mode);
            assert!(pods.poll(now).is_empty(), "{mode}: no 0x4B");
            assert_eq!(pods.next_due(), Some(now + ECHO_DELAY), "{mode}");
            assert!(pods.poll(now + ECHO_DELAY - ms(1)).is_empty());
            assert_eq!(
                sent(&pods.poll(now + ECHO_DELAY)),
                [noise::set_packet(mode).to_vec()]
            );
        }
    }

    /// AirPods 4's answers, through the quirks: the 0x4B notification on every
    /// change; the echo on every fourth, after the delay, carrying the mode.
    #[test]
    fn answers_mode_changes_as_airpods_4_do() {
        let mut pods = subscribed();
        pods.quirks.settings_changed = true;
        pods.quirks.echo_every = 4;
        pods.quirks.echo_delay = ms(500);
        pods.quirks.stale_report = None;
        let delay = pods.quirks.echo_delay;
        let modes = [
            NoiseMode::Transparency,
            NoiseMode::Adaptive,
            NoiseMode::NoiseCancellation,
            NoiseMode::Transparency,
        ];
        for (i, mode) in modes.into_iter().enumerate() {
            let now = ms(1000 * (i as u64 + 1));
            pods.receive(&noise::set_packet(mode), now);
            assert_eq!(pods.noise_mode(), mode);
            assert_eq!(
                sent(&pods.poll(now)),
                [noise::settings_changed_packet().to_vec()],
                "change {}",
                i + 1
            );
            if i < 3 {
                assert_eq!(pods.next_due(), None);
                assert!(pods.poll(now + delay).is_empty());
            } else {
                assert_eq!(pods.next_due(), Some(now + delay));
                assert!(pods.poll(now + delay - ms(1)).is_empty());
                assert_eq!(
                    sent(&pods.poll(now + delay)),
                    [noise::set_packet(mode).to_vec()]
                );
            }
        }
    }

    /// Off from Noise Cancellation as the #71 capture has it - a report of
    /// Noise Cancellation at +0.33 s, the default, and the echo of Off at +1.4
    /// s. No other mode brings such a report, nor does Off from Off.
    #[test]
    fn off_brings_a_report_of_the_mode_it_replaced() {
        let mut pods = subscribed();
        pods.quirks.echo_delay = ms(1400);
        pods.receive(&noise::allow_off_packet(true), ms(1000));
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(1000));
        assert!(pods.poll(ms(1000)).is_empty());
        assert_eq!(pods.next_due(), Some(ms(1330)));
        assert_eq!(
            sent(&pods.poll(ms(1330))),
            [noise::set_packet(NoiseMode::NoiseCancellation).to_vec()]
        );
        assert_eq!(pods.noise_mode(), NoiseMode::Off, "the report is stale");
        assert_eq!(
            sent(&pods.poll(ms(2400))),
            [noise::set_packet(NoiseMode::Off).to_vec()]
        );

        let modes = [NoiseMode::Off, NoiseMode::Transparency, NoiseMode::Adaptive];
        for (i, mode) in modes.into_iter().enumerate() {
            let now = ms(3000 * (i as u64 + 1));
            pods.receive(&noise::set_packet(mode), now);
            pods.poll(now);
            let echo = now + ms(1400);
            assert_eq!(pods.next_due(), Some(echo), "{mode}: the echo alone");
            assert_eq!(sent(&pods.poll(echo)), [noise::set_packet(mode).to_vec()]);
        }
    }

    #[test]
    fn the_echo_and_the_notification_can_be_switched() {
        let mut pods = subscribed();
        pods.quirks.settings_changed = true;
        pods.quirks.echo_delay = Duration::ZERO;
        pods.receive(&noise::set_packet(NoiseMode::Adaptive), ms(10));
        assert_eq!(
            sent(&pods.poll(ms(10))),
            [
                noise::settings_changed_packet().to_vec(),
                noise::set_packet(NoiseMode::Adaptive).to_vec()
            ]
        );

        pods.quirks.echo_every = 0;
        pods.quirks.settings_changed = false;
        for mode in NoiseMode::ALL {
            pods.receive(&noise::set_packet(mode), ms(20));
        }
        assert!(pods.poll(ms(10_000)).is_empty());
    }

    /// The ear report, `primary` then `secondary`.
    fn ears(primary: Place, secondary: Place) -> Vec<u8> {
        ear_packet(EarReport { primary, secondary }).to_vec()
    }

    /// What the handshake brings: its answer, then the primary pod and the
    /// ear report, unasked.
    fn handshake_answer(pods: &Pods) -> Vec<Vec<u8>> {
        let ears = pods.ears();
        vec![
            HANDSHAKE_ACK.to_vec(),
            ears.primary_packet(false),
            ear_packet(ears.report).to_vec(),
        ]
    }

    /// Out of the box: both pods in the open case, the left one primary.
    #[test]
    fn tells_where_the_pods_are_once_the_handshake_is_answered() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.aap_opened(ms(0));
        pods.set_in_ear(PodSide::Left, true);
        assert!(pods.poll(ms(0)).is_empty(), "nothing before the handshake");

        pods.receive(&HANDSHAKE, ms(0));
        assert_eq!(
            sent(&pods.poll(ms(0))),
            [
                HANDSHAKE_ACK.to_vec(),
                primary_packet(PodSide::Left, true, false).to_vec(),
                ears(Place::InEar, Place::InCase),
            ]
        );
        assert!(pods.poll(ms(10)).is_empty());
    }

    /// A pod in or out of an ear or the case, the primary pod's place first.
    #[test]
    fn reports_where_the_pods_go() {
        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.set_in_ear(PodSide::Right, true);
        assert_eq!(
            sent(&pods.poll(ms(10))),
            [ears(Place::InEar, Place::InEar)],
            "one report for both changes"
        );
        pods.set_in_ear(PodSide::Right, false);
        assert_eq!(sent(&pods.poll(ms(20))), [ears(Place::InEar, Place::Out)]);
        pods.set_in_case(PodSide::Right, true);
        assert_eq!(
            sent(&pods.poll(ms(30))),
            [ears(Place::InEar, Place::InCase)]
        );
        pods.set_in_ear(PodSide::Right, true);
        pods.set_in_ear(PodSide::Right, true);
        assert_eq!(sent(&pods.poll(ms(40))), [ears(Place::InEar, Place::InEar)]);
        assert!(pods.poll(ms(50)).is_empty(), "no change, no packet");
    }

    /// The #142 captures' switch: the ear report in the new order first, then
    /// the primary pod twice, the first marked as a switch.
    #[test]
    fn a_role_switch_sends_the_ear_report_first() {
        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.set_in_ear(PodSide::Right, true);
        pods.poll(ms(0));
        pods.set_in_ear(PodSide::Left, false);
        assert_eq!(sent(&pods.poll(ms(0))), [ears(Place::Out, Place::InEar)]);

        let switched = ms(900);
        pods.set_primary(PodSide::Right);
        assert_eq!(
            sent(&pods.poll(switched)),
            [
                battery_packet(&pods.aap_batteries()),
                ears(Place::InEar, Place::Out)
            ],
            "the battery too, the new primary first"
        );
        let first = switched + PRIMARY_AFTER_EAR;
        assert_eq!(pods.next_due(), Some(first));
        assert!(pods.poll(first - ms(1)).is_empty());
        assert_eq!(
            sent(&pods.poll(first)),
            [primary_packet(PodSide::Right, true, true).to_vec()]
        );
        assert_eq!(
            sent(&pods.poll(first + SECOND_PRIMARY)),
            [primary_packet(PodSide::Right, true, false).to_vec()]
        );
        assert_eq!(pods.next_due(), None);
    }

    /// With both pods worn the report reads the same either way round, and
    /// only the primary pod packets come.
    #[test]
    fn a_switch_with_both_pods_worn_sends_the_primary_alone() {
        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.set_in_ear(PodSide::Right, true);
        pods.poll(ms(0));
        pods.set_primary(PodSide::Right);
        assert_eq!(
            sent(&pods.poll(ms(0))),
            [battery_packet(&pods.aap_batteries())],
            "no ear report"
        );
        assert_eq!(
            sent(&pods.poll(PRIMARY_AFTER_EAR)),
            [primary_packet(PodSide::Right, true, true).to_vec()]
        );
    }

    /// The secondary pod in the case as the lid closes and opens: the place,
    /// the primary pod packet's say on whether it can be reached, and the
    /// battery, which reports that pod and the case Disconnected while the lid
    /// is closed.
    #[test]
    fn the_closed_case_is_reported() {
        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.poll(ms(0));
        pods.set_lid(false);
        let away = [
            battery(Component::Left, Some(80), Status::Discharging),
            battery(Component::Right, Some(0), Status::Disconnected),
            battery(Component::Case, Some(0), Status::Disconnected),
        ];
        assert_eq!(
            sent(&pods.poll(ms(10))),
            [
                battery_packet(&away),
                ears(Place::InEar, Place::InClosedCase),
                primary_packet(PodSide::Left, false, false).to_vec(),
            ]
        );
        pods.set_lid(true);
        let back = [
            battery(Component::Left, Some(80), Status::Discharging),
            battery(Component::Right, Some(70), Status::Discharging),
            battery(Component::Case, Some(50), Status::Discharging),
        ];
        assert_eq!(
            sent(&pods.poll(ms(20))),
            [
                battery_packet(&back),
                ears(Place::InEar, Place::InCase),
                primary_packet(PodSide::Left, true, false).to_vec(),
            ]
        );
    }

    fn battery(component: Component, level: Option<u8>, status: Status) -> Battery {
        Battery {
            component,
            level,
            status,
        }
    }

    /// A part the model holds as unknown goes out as the hardware sends one
    /// that is away: status Disconnected at level 0 - `08 01 00 04 01` for the
    /// case, captured with both pods worn (#71) - which the app reads as
    /// unknown, not 0% (#108).
    #[test]
    fn a_part_unknown_is_sent_disconnected_at_level_0() {
        let mut pods = subscribed();
        pods.set_battery(Part::Case, None);
        let packets = sent(&pods.poll(ms(10)));
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][17..], [0x08, 0x01, 0x00, 0x04, 0x01]);
        let info = parse_battery_packet(&packets[0]).unwrap();
        assert_eq!(info.case.unwrap().level, None);
        assert_eq!(info.left.unwrap().level, Some(80));
    }

    /// The primary pod first, then the other, then the case - and after a
    /// role switch the new primary first, which is a change to report.
    #[test]
    fn the_battery_lists_the_primary_pod_first() {
        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.set_in_ear(PodSide::Right, true);
        pods.poll(ms(0));
        let components = |packet: &[u8]| [packet[7], packet[12], packet[17]];
        pods.set_battery(Part::Left, Some(60));
        let left_first = sent(&pods.poll(ms(10)));
        assert_eq!(components(&left_first[0]), [0x04, 0x02, 0x08]);

        pods.set_primary(PodSide::Right);
        let right_first = sent(&pods.poll(ms(20)));
        assert_eq!(right_first.len(), 1, "{right_first:02x?}");
        assert_eq!(components(&right_first[0]), [0x02, 0x04, 0x08]);
        let info = parse_battery_packet(&right_first[0]).unwrap();
        assert_eq!(info.left.unwrap().level, Some(60));
    }

    #[test]
    fn a_battery_change_is_notified() {
        let mut pods = subscribed();
        pods.set_battery(Part::Left, Some(50));
        pods.set_charging(Part::Right, true);
        let packets = sent(&pods.poll(ms(10)));
        assert_eq!(packets.len(), 1, "one packet for both changes");
        let info: BatteryInfo = parse_battery_packet(&packets[0]).unwrap();
        assert_eq!(info.left.unwrap().level, Some(50));
        assert!(info.right.unwrap().is_charging());

        assert!(pods.poll(ms(20)).is_empty());
        pods.set_battery(Part::Left, Some(50));
        assert!(pods.poll(ms(30)).is_empty(), "no change, no packet");
    }

    #[test]
    fn nothing_is_notified_before_the_app_subscribes() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.poll(ms(0));
        pods.set_battery(Part::Left, Some(10));
        pods.set_noise_mode(NoiseMode::Adaptive);
        assert!(pods.poll(ms(10)).is_empty());
    }

    #[test]
    fn a_mode_changed_elsewhere_is_reported() {
        let mut pods = subscribed();
        pods.set_noise_mode(NoiseMode::Transparency);
        assert_eq!(
            sent(&pods.poll(ms(10))),
            [noise::set_packet(NoiseMode::Transparency).to_vec()]
        );
        assert!(pods.poll(ms(20)).is_empty());
    }

    #[test]
    fn the_silent_pair_never_answers_and_hangs_up() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.silent_handshake = true;
        let opened = ms(1000);
        pods.aap_opened(opened);
        for packet in [&HANDSHAKE[..], &NOTIFICATIONS, &FEATURES, &KEY_REQUEST] {
            pods.receive(packet, opened);
        }
        pods.set_battery(Part::Left, Some(5));
        assert!(pods.poll(opened).is_empty());
        assert_eq!(pods.next_due(), Some(opened + SILENT_HANG_UP));
        assert!(pods.poll(opened + SILENT_HANG_UP - ms(1)).is_empty());
        assert_eq!(pods.poll(opened + SILENT_HANG_UP), [Action::HangUp]);
        assert!(!pods.has_aap());
        assert_eq!(pods.received().len(), 4);
    }

    /// Each connection turned away counts down the quirk, and the one after
    /// the last is taken.
    #[test]
    fn a_refusing_pair_turns_away_as_many_connections_as_asked() {
        let mut pods = Pods::new(PRO_3, MAC);
        assert!(!pods.refuses_aap(), "takes every connection by default");
        pods.quirks.refuse_aap = 2;
        assert!(pods.refuses_aap());
        assert!(pods.refuses_aap());
        assert!(!pods.refuses_aap());
        assert_eq!(pods.quirks.refuse_aap, 0);
    }

    /// The battery and the startup dump wait for the report delay, and come
    /// together once it is over.
    #[test]
    fn the_report_waits_for_its_delay() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.report_delay = ms(500);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.receive(&NOTIFICATIONS, ms(0));
        assert_eq!(sent(&pods.poll(ms(0))), handshake_answer(&pods));
        assert_eq!(pods.next_due(), Some(ms(500)));
        assert!(pods.poll(ms(499)).is_empty());
        let report = sent(&pods.poll(ms(500)));
        assert!(parse_battery_packet(&report[0]).is_ok(), "{report:02x?}");
        assert_eq!(report.len(), 3 + OTHER_SETTINGS.len());
    }

    /// Switched on during a connection, the quirk waits for the next one.
    #[test]
    fn a_connection_does_not_turn_silent_halfway() {
        let mut pods = subscribed();
        pods.quirks.silent_handshake = true;
        pods.receive(&KEY_REQUEST, ms(10));
        assert_eq!(pods.poll(ms(10)).len(), 1);
    }

    #[test]
    fn disconnecting_hangs_up() {
        let mut pods = subscribed();
        pods.set_noise_mode(NoiseMode::Adaptive);
        pods.set_connected(false);
        assert_eq!(pods.poll(ms(10)), [Action::HangUp]);
        assert!(!pods.has_aap());
        assert!(pods.poll(ms(20)).is_empty());
        let pd = parse_proximity_data(&pods.advertisement(ms(20)).data).unwrap();
        assert_eq!(pd.connection_state, CONNECTION_NONE);
    }

    /// What was due on a connection the app closed goes with it.
    #[test]
    fn closing_drops_what_was_due() {
        let mut pods = subscribed();
        pods.quirks.echo_every = 1;
        pods.receive(&noise::set_packet(NoiseMode::Adaptive), ms(10));
        pods.aap_closed();
        assert!(pods.is_connected(), "the Bluetooth link stays");
        assert_eq!(pods.next_due(), None);
        assert!(pods.poll(ms(10_000)).is_empty());
    }

    #[test]
    fn records_what_the_app_sends() {
        let mut pods = subscribed();
        let off = [
            noise::allow_off_packet(true).to_vec(),
            noise::set_packet(NoiseMode::Off).to_vec(),
        ];
        for packet in &off {
            pods.receive(packet, ms(10));
        }
        let received = pods.received();
        assert_eq!(received.len(), 6);
        assert_eq!(received[0], HANDSHAKE);
        assert_eq!(received[3], KEY_REQUEST);
        assert_eq!(received[4..], off);
    }

    #[test]
    #[should_panic(expected = "0-100%")]
    fn a_battery_above_100_is_refused() {
        Pods::new(PRO_3, MAC).set_battery(Part::Case, Some(101));
    }
}
