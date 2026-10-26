//! A pair of fake AirPods: the state of one pair, and the bytes it sends.
//!
//! Pure - state in, bytes out. There are no sockets, no D-Bus and no clock
//! here: time comes in as an argument, an offset from an epoch the caller
//! picks - the fake world's start, say - and must not run backwards. That
//! keeps every behaviour testable without waiting, the timed ones included,
//! and leaves the I/O to the fake BlueZ and the world around it (#72).
//!
//! Everything sent goes through the builders beside the app's parsers -
//! `battery_packet`, `key_packet`, `Cleartext`, `Plaintext` and the rest - so
//! the fake cannot encode the protocol differently from how the app reads it.
//!
//! A pair talks on two channels, as real AirPods do:
//!
//! - **BLE.** [`Pods::advertisement`] is what the pair broadcasts at a moment:
//!   the Apple manufacturer data, from a random address that rotates.
//! - **AAP.** The world hands the model what the app sends
//!   ([`Pods::receive`]) and carries out whatever [`Pods::poll`] returns.
//!   Replies, notifications and the timed quirks all come out of `poll`.
//!
//! Where the hardware's behaviour is not recorded, the fake makes a choice and
//! its documentation says so; the reference captures of #71 are what can
//! correct it.

use std::time::Duration;

use bluer::Address;

use crate::aap::battery::{Battery, Component, Status, battery_packet};
use crate::aap::keys::{KeyType, ProximityKey, key_packet};
use crate::aap::noise::{self, NoiseMode};
use crate::ble::decrypt::{CONNECTED_SUFFIX, KEY_LEN, Plaintext, encrypt, random_key};
use crate::ble::parser::{Cleartext, PodSide, proximity_message};

/// AirPods Pro 3, by the product ID `parse_proximity_data` reads.
pub const PRO_3: u16 = 0x2027;
/// AirPods Pro 2 with USB-C.
pub const PRO_2_USB_C: u16 = 0x2024;

/// How long a pair advertises from one address. Apple rotates it every few
/// seconds - four addresses for one pair were seen inside 45 seconds.
pub const ADDRESS_ROTATION: Duration = Duration::from_secs(10);

/// How long a pair with the silent quirk holds a connection before hanging up,
/// as the one time it was seen.
pub const SILENT_HANG_UP: Duration = Duration::from_secs(9);

/// Where an AAP packet carries its command.
const OPCODE: usize = 4;

// The requests `aap::client` sends, known by their command byte. The packets
// themselves are private to the client.
const OP_HANDSHAKE: u8 = 0x01;
const OP_NOTIFICATIONS: u8 = 0x0F;
const OP_FEATURES: u8 = 0x4D;
const OP_KEY_REQUEST: u8 = 0x30;

/// The answer to the handshake. Pro 3 sends 18 bytes, starting
/// `01 00 04 00 00 00 01 00`; the app, like LibrePods, matches only these
/// four, so the fake sends just them.
const HANDSHAKE_ACK: [u8; 4] = [0x01, 0x00, 0x04, 0x00];

/// The answer to the features packet, as LibrePods recognises it - by these
/// six bytes, and tested there with AirPods Pro 2 only.
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

/// Behaviour of the hardware that the fake can switch. The default is what the
/// hardware does.
///
/// Each one is read when it applies, so a change takes effect from the next
/// packet, advertisement or connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirks {
    /// Ignore a bare Off until `0x34 01` has allowed it: no mode change and
    /// nothing on the wire, where recent firmware on Pro 3 and Pro 2 plays an
    /// error chime.
    pub off_needs_allow: bool,
    /// Answer every mode change with the 0x4B settings-changed notification.
    pub settings_changed: bool,
    /// Echo every n-th mode change with a 0x0D report of the mode set: 1
    /// echoes every change, 0 none. Counted rather than drawn at random, so
    /// tests are never flaky; the default 4 is the hardware's one change in
    /// four, the first echo following the fourth change.
    pub echo_every: u32,
    /// How long after its change an echo comes. The captures record that it
    /// comes, not when; the default puts it after the app has recorded the mode
    /// it asked for, so the echo confirms rather than sets it.
    pub echo_delay: Duration,
    /// Zero the MAC suffix in the encrypted payload while connected to a host,
    /// as Pro 3 does.
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
    /// one is not recorded - a busy pair, a link not up yet - so this is a
    /// failed connect at its plainest.
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
            settings_changed: true,
            echo_every: 4,
            echo_delay: Duration::from_millis(500),
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
    /// real address.
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
    /// cancelling, Off not allowed, and connected to nothing.
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
            noise_mode: NoiseMode::NoiseCancelling,
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
    /// case.
    pub fn set_in_ear(&mut self, side: PodSide, in_ear: bool) {
        let pod = self.pod_mut(side);
        pod.in_ear = in_ear;
        pod.in_case &= !in_ear;
    }

    /// Puts a pod in the case, or takes it out. A pod in the case is out of
    /// the ear.
    pub fn set_in_case(&mut self, side: PodSide, in_case: bool) {
        let pod = self.pod_mut(side);
        pod.in_case = in_case;
        pod.in_ear &= !in_case;
    }

    /// Opens or closes the case. The advertisement carries the lid only while
    /// the pod that sends it is in the case, or both are. With the sending pod
    /// worn and the other in the case, it says only that a pod is in the case,
    /// and the app keeps the lid it last heard.
    pub fn set_lid(&mut self, open: bool) {
        self.lid_open = open;
    }

    pub fn set_primary(&mut self, side: PodSide) {
        self.primary = side;
    }

    pub fn set_color(&mut self, color: u8) {
        self.color = color;
    }

    /// Connects the pair to a host, or disconnects it - this one or another,
    /// such as a phone. A connected pair zeroes the suffix in its advertisement
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
    /// three bytes in bytes 7-9 - zeroed while connected - and a tail that
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
            CONNECTED_SUFFIX
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
    /// It has the top bits, 01, of a resolvable private address - the kind
    /// the IRK exists to resolve - but is not derived the way the Bluetooth
    /// specification derives one, so the IRK does not resolve it. The app never
    /// tries.
    pub fn address_at(&self, now: Duration) -> Address {
        let period = (now.as_millis() / ADDRESS_ROTATION.as_millis()) as u64;
        let bytes = self.derive(b'A', period);
        let mut address = [0u8; 6];
        address.copy_from_slice(&bytes[..6]);
        address[0] = address[0] & 0x3F | 0x40;
        Address::new(address)
    }

    /// The app opened an AAP connection at `now`. There is no AAP without a
    /// Bluetooth link, so the pair is connected from here on.
    pub fn aap_opened(&mut self, now: Duration) {
        self.connected = true;
        let mut session = Session {
            silent: self.quirks.silent_handshake,
            reported: None,
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
    /// - the handshake gets its acknowledgement;
    /// - the notification request (0x0F) the battery and the startup dump - the
    ///   mode, allow-Off and other 0x09 settings - and from then on a report of
    ///   every change;
    /// - the features packet (0x4D) its acknowledgement;
    /// - the key request (0x30) the key packet, IRK then ENC_KEY;
    /// - a mode change the 0x4B notification and now and then the delayed echo,
    ///   or nothing for an Off not allowed - see [`Quirks`];
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
            Some(OP_HANDSHAKE) => self.send(now, HANDSHAKE_ACK.to_vec()),
            Some(OP_FEATURES) => self.send(now, FEATURES_ACK.to_vec()),
            Some(OP_NOTIFICATIONS) => self.start_notifications(now),
            Some(OP_KEY_REQUEST) => self.send(now, self.key_packet()),
            _ => self.settings_command(packet, now),
        }
    }

    /// What is due on the AAP connection by `now`, in order: replies and timed
    /// actions as they fall due, then a report of each change the app has not
    /// been told of - batteries and charging in one battery packet, the mode in
    /// a 0x0D report. Taken once; a second call at the same time returns
    /// nothing new.
    ///
    /// A [`Action::HangUp`] ends the connection, and nothing comes after it.
    pub fn poll(&mut self, now: Duration) -> Vec<Action> {
        let batteries = self.aap_batteries();
        let mode = self.noise_mode;
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
        actions
    }

    /// When the next timed action falls due - a delayed echo, or the silent
    /// pair's hang-up - for the caller to [`Pods::poll`] at. `None` with
    /// nothing scheduled. Replies and changes made through the setters are due
    /// at once and not counted here: poll after each.
    pub fn next_due(&self) -> Option<Duration> {
        self.session.as_ref()?.scheduled.first().map(|(at, _)| *at)
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

    /// The batteries as AAP reports them, in the order the device lists them:
    /// right, left, case.
    fn aap_batteries(&self) -> [Battery; 3] {
        let battery = |component, level, charging| Battery {
            component,
            level,
            status: if charging {
                Status::Charging
            } else {
                Status::Discharging
            },
        };
        [
            battery(Component::Right, self.right.battery, self.right.charging),
            battery(Component::Left, self.left.battery, self.left.charging),
            battery(Component::Case, self.case_battery, self.case_charging),
        ]
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

        self.noise_mode = mode;
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
        assert_eq!(plain[7..10], CONNECTED_SUFFIX);
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

    #[test]
    fn the_address_rotates() {
        let pods = Pods::new(PRO_3, MAC);
        let first = pods.address_at(ms(0));
        assert_eq!(pods.address_at(ADDRESS_ROTATION - ms(1)), first);
        let second = pods.address_at(ADDRESS_ROTATION);
        assert_ne!(second, first);
        assert_eq!(pods.advertisement(ADDRESS_ROTATION).address, second);
        for address in [first, second] {
            assert_ne!(address, MAC);
            assert_eq!(address[0] >> 6, 0b01, "{address}");
        }

        let other = Pods::new(PRO_3, MAC);
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
        assert_eq!(sent(&pods.poll(ms(0))), [HANDSHAKE_ACK.to_vec()]);

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
        assert_eq!(pods.noise_mode(), NoiseMode::NoiseCancelling);

        pods.receive(&noise::allow_off_packet(true), ms(20));
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(20));
        assert!(pods.allow_off());
        assert_eq!(pods.noise_mode(), NoiseMode::Off);
        assert_eq!(
            sent(&pods.poll(ms(20))),
            [noise::settings_changed_packet().to_vec()]
        );
    }

    #[test]
    fn off_needs_no_allowing_without_the_quirk() {
        let mut pods = subscribed();
        pods.quirks.off_needs_allow = false;
        pods.receive(&noise::set_packet(NoiseMode::Off), ms(10));
        assert_eq!(pods.noise_mode(), NoiseMode::Off);
    }

    /// The 0x4B notification on every change; the echo on every fourth, after
    /// the delay, carrying the mode.
    #[test]
    fn answers_mode_changes_as_the_hardware_does() {
        let mut pods = subscribed();
        let delay = pods.quirks.echo_delay;
        let modes = [
            NoiseMode::Transparency,
            NoiseMode::Adaptive,
            NoiseMode::NoiseCancelling,
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

    #[test]
    fn the_echo_and_the_notification_can_be_switched() {
        let mut pods = subscribed();
        pods.quirks.echo_every = 1;
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
        assert_eq!(sent(&pods.poll(ms(0))), [HANDSHAKE_ACK.to_vec()]);
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
