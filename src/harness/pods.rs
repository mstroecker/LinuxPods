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
//! - **BLE.** [`Pods::advertisements`] is what the pair broadcasts at a
//!   moment: the Apple manufacturer data from each pod that advertises, each
//!   from a resolvable private address of its own that the pair's IRK
//!   resolves, and that rotates.
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
use crate::aap::conversation;
use crate::aap::ear::{EarReport, Place, ear_packet, primary_packet};
use crate::aap::info;
use crate::aap::keys::{KeyType, ProximityKey, key_packet};
use crate::aap::message::HEADER;
use crate::aap::name;
use crate::aap::noise::{self, NoiseMode};
use crate::ble::decrypt::{KEY_LEN, Plaintext, ZEROED_SUFFIX, encrypt, random_key};
use crate::ble::parser::{Cleartext, PodSide, proximity_message};
use crate::ble::rpa::resolvable_address;

/// AirPods Pro 3, by the product ID `parse_proximity_data` reads.
pub const PRO_3: u16 = 0x2027;
/// AirPods Pro 2 with USB-C.
pub const PRO_2_USB_C: u16 = 0x2024;

/// How long a pod advertises from one address. Real AirPods advertise from
/// an address per pod, which in the #71 captures lasted 4 to 8 minutes,
/// connected or not, each pod on a clock of its own. The fake rotates far
/// faster on purpose: it is there to put rotation through its paces in
/// tests. The left pod's clock runs half a rotation off the right's, so the
/// two never change address together on the clock, only as they come back
/// from the closed case.
pub const ADDRESS_ROTATION: Duration = Duration::from_secs(10);

/// How long after the lid closes a pod in the case stops advertising: 3.9 s
/// and 22 advertisements in the #71 captures, both pods in the case or one,
/// the other worn. The real value: nothing waits on it but a test that asks
/// for it.
pub const LID_SILENCE: Duration = Duration::from_secs(4);

/// How long after the lid opens on pods gone quiet in the case the right pod
/// advertises again: 0.94-1.03 s in the #71 captures. The real value.
pub const RIGHT_RESUMES: Duration = Duration::from_secs(1);

/// The same for the left pod: 5.4-5.6 s in the #71 captures. The real value.
pub const LEFT_RESUMES: Duration = Duration::from_millis(5500);

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
/// primary pod packet comes. The ear report came before it at every switch:
/// 0.2-31 ms on a Pro 3 in the #142 captures, 0.5-87 ms on a Pro 2 (USB-C) on
/// 2026-11-02 (#71).
pub const PRIMARY_AFTER_EAR: Duration = Duration::from_millis(10);

/// How long after a role switch's first primary pod packet its second one
/// comes: 31-88 ms on a Pro 3 in the #142 capture that shows it, 28-291 ms on
/// a Pro 2 (USB-C) on 2026-11-02 (#71).
pub const SECOND_PRIMARY: Duration = Duration::from_millis(60);

// A stretch of speech with Conversation Awareness on, as the first of the
// #150 run went: the real values, each from the start of the speech - level
// 01 - or from its end, level 03. See `Pods::speak`.

/// From the start to the report of the mode the pods are in: 0.02 s.
pub const SPEECH_MODE_REPORT: Duration = Duration::from_millis(20);
/// From the start to the pods' switch to Transparency: 0.31 s.
pub const SPEECH_TRANSPARENCY: Duration = Duration::from_millis(310);
/// From the start to level 02, still speaking: 0.82 s.
pub const SPEECH_STILL: Duration = Duration::from_millis(820);
/// From the end, level 03, to 07 and 08, the way back up: 0.07 s and 0.10 s.
pub const SPEECH_RAMP: [(Duration, u8); 2] = [
    (Duration::from_millis(70), 0x07),
    (Duration::from_millis(100), 0x08),
];
/// From the end to the pods' switch back to the user's mode: 0.46 s.
pub const SPEECH_MODE_BACK: Duration = Duration::from_millis(460);
/// From the end to level 09, idle: 0.48 s.
pub const SPEECH_IDLE: Duration = Duration::from_millis(480);

/// The speech levels the fake sends - see `aap::conversation::Speech`.
const LEVEL_SPEAKING: u8 = 0x01;
const LEVEL_STILL_SPEAKING: u8 = 0x02;
const LEVEL_SPEECH_ENDED: u8 = 0x03;
const LEVEL_NORMAL: u8 = 0x08;
const LEVEL_IDLE: u8 = 0x09;

// The requests a link sends as it comes up - `aap::Client::open`'s, and the key
// request `Coordinator::open_aap` adds - known by their command byte, as a
// device would know them: not by the packets beside the app's parsers, which a
// change on the wire would change here along with them.
const OP_HANDSHAKE: u8 = 0x01;
const OP_NOTIFICATIONS: u8 = 0x0F;
const OP_KEY_REQUEST: u8 = 0x30;

/// The name the pods are given out of the box: what [`Pods::new`] holds
/// until [`Pods::set_name`] or a rename changes it.
pub const DEFAULT_NAME: &str = "AirPods Pro";

/// The longest name the pods keep, in bytes: a Pro 3 cut a longer one to its
/// first 127 bytes, silently, in the name run of #160. The real value.
pub const NAME_KEPT: usize = 127;

/// The Marketing Version the pods send in the device information packet,
/// field 10, until [`Pods::set_marketing_version`] changes it: a Pro 3's
/// and a Pro 2 (USB-C)'s on 2026-11-02, which the iPhone showed as `9A348`
/// (#161). The real value; the fields around it are made up.
pub const MARKETING_VERSION: u32 = 9_442_752;

/// The answer to the handshake, all 18 bytes, as a Pro 3 sent it in every #71
/// capture. The app, like LibrePods, matches the first four.
const HANDSHAKE_ACK: [u8; 18] = [
    0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00,
];

/// The address the fake gives this host, the one the app runs on, where AAP
/// packets name a host: the second 0x0C, 0x0E, 0x2E and the 0x2B list. The
/// fake cannot know the real one, and the app reads none of them; it is from
/// the range set aside for documentation, as the tests' pairs are.
pub const THIS_HOST: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0xF0]);

/// Another host the pair has known, for the 0x2B list.
const ANOTHER_HOST: Address = Address::new([0x00, 0x00, 0x5E, 0x00, 0x53, 0xF1]);

/// How long after the first 0x0C the second one comes by default, naming this
/// host, and the startup dump right after it - see [`Quirks::dump_delay`]. A
/// Pro 3 took 1.4-1.5 s in the #71 captures; the fake takes far less, as
/// every test that waits for the mode would wait that long, and any wait at
/// all keeps the order: the battery and the ears first, the mode later.
pub const DUMP_DELAY: Duration = Duration::from_millis(200);

/// What the startup dump holds, in the order a Pro 3 sent it in the #71
/// captures. Settings of the 0x09 family carry the values captured: those
/// `docs/aap-noise-control.md` records, and capture 4's for the rest. Several
/// are valid mode bytes - 0x25's 01 reads as Off, 0x26's 02 as Noise
/// Cancellation - so taking a mode from any 0x09 packet without checking byte
/// 6 reads them as one. Neither 0x34, allow-Off, nor 0x1A was ever in it.
/// Conversation Awareness, 0x28, carries the pair's setting: `02` in every
/// #71 capture, `01` in the #150 run.
const STARTUP_DUMP: [Dumped; 30] = [
    Dumped::Other(0x02),
    Dumped::DeviceInfo,
    Dumped::Mode,
    Dumped::Setting(0x18, &[0x00]),
    Dumped::Setting(0x17, &[0x00]),
    Dumped::Setting(0x25, &[0x01]),
    Dumped::Setting(0x23, &[0x00]),
    Dumped::Setting(0x1F, &[0x50, 0x50]),
    Dumped::Setting(0x24, &[0x00, 0x03]),
    Dumped::ConversationAwareness,
    Dumped::Other(0x63),
    Dumped::Other(0x4E),
    Dumped::Setting(0x26, &[0x02]),
    Dumped::Setting(0x29, &[0x02]),
    Dumped::Setting(0x2C, &[0x02]),
    Dumped::Setting(0x2F, &[0x00]),
    Dumped::Setting(0x33, &[0x00]),
    Dumped::Setting(0x2E, &[0x32]),
    Dumped::Setting(0x30, &[0x01]),
    Dumped::Setting(0x1B, &[0x02]),
    Dumped::Setting(0x35, &[0x01]),
    Dumped::Setting(0x3E, &[0x02]),
    Dumped::Setting(0x37, &[0x00]),
    Dumped::Setting(0x38, &[0x52]),
    Dumped::Setting(0x3B, &[0x01]),
    Dumped::Other(0x53),
    Dumped::Other(0x55),
    Dumped::Other(0x17),
    Dumped::Other(0x17),
    Dumped::Other(0x17),
];

/// One packet of the startup dump.
#[derive(Debug, Clone, Copy)]
enum Dumped {
    /// The noise control mode, which the app reads.
    Mode,
    /// Conversation Awareness, which the app reads too.
    ConversationAwareness,
    /// Another setting of the 0x09 family: its sub-command and value.
    Setting(u8, &'static [u8]),
    /// 0x1D, the device information: the pair's name, then made-up stand-ins
    /// for the model number, the serial numbers and the rest, and the
    /// pair's Marketing Version in field 10, laid out as a Pro 3's.
    DeviceInfo,
    /// A packet nothing reads, by its opcode: a short stand-in for one that
    /// ran to hundreds of bytes on the hardware.
    Other(u8),
}

impl Dumped {
    fn packet(
        self,
        mode: NoiseMode,
        conversation_awareness: bool,
        name: &[u8],
        marketing_version: u32,
    ) -> Vec<u8> {
        match self {
            // The mode packet serves both directions: this is the report.
            Self::Mode => noise::set_packet(mode).to_vec(),
            // So does the setting's.
            Self::ConversationAwareness => {
                conversation::set_packet(conversation_awareness).to_vec()
            }
            Self::Setting(sub_cmd, value) => noise::settings_report(sub_cmd, value).to_vec(),
            Self::DeviceInfo => {
                info::info_packet_with_version(name, &marketing_version.to_string())
            }
            Self::Other(opcode) => {
                let mut packet = header(opcode);
                packet.extend_from_slice(&[0x00, 0x00]);
                packet
            }
        }
    }
}

/// The AAP header, an opcode and its zero byte: how every packet the pair
/// sends after the handshake's answer starts.
fn header(opcode: u8) -> Vec<u8> {
    let mut packet = HEADER.to_vec();
    packet.extend_from_slice(&[opcode, 0x00]);
    packet
}

/// An address as 0x0C, 0x0E and the 0x2B list carry it: least significant
/// byte first. 0x2E carries it the other way round, as `Address` holds it.
fn reversed(address: Address) -> [u8; 6] {
    let mut bytes = address.0;
    bytes.reverse();
    bytes
}

/// 0x0C, 14 bytes: an address, then `01 02` for a bud or `01 01` for this
/// host. It names the device the pods take to be in use, most likely: the
/// first after the notification request named one of the buds, the second
/// this host, 1.4-1.5 s later, and the startup dump followed the second
/// within a millisecond, in every #71 capture.
fn in_use_packet(address: Address, this_host: bool) -> Vec<u8> {
    let mut packet = header(0x0C);
    packet.extend_from_slice(&reversed(address));
    packet.extend_from_slice(&[0x01, if this_host { 0x01 } else { 0x02 }]);
    packet
}

/// 0x0E, 13 bytes, naming this host right after the first 0x0C.
fn host_packet(address: Address) -> Vec<u8> {
    let mut packet = header(0x0E);
    packet.extend_from_slice(&reversed(address));
    packet.push(0x00);
    packet
}

/// 0x2E, 17 bytes: one entry, this host's address most significant byte
/// first, and `02 02`.
fn host_entry_packet(address: Address) -> Vec<u8> {
    let mut packet = header(0x2E);
    packet.extend_from_slice(&[0x01, 0x00, 0x01]);
    packet.extend_from_slice(&address.0);
    packet.extend_from_slice(&[0x02, 0x02]);
    packet
}

/// 0x2B, the hosts the pair knows, which comes unasked with the handshake's
/// answer - not in answer to the features packet, which nothing answers
/// (#71). As captured: `01`, the length of the entries, 8 bytes, then 17
/// bytes an entry, each 8 bytes, an address least significant byte first and
/// 3 bytes. The 8-byte fields are synthetic.
fn known_hosts_packet(hosts: &[Address]) -> Vec<u8> {
    const ENTRY_LEN: usize = 17;
    let len = u16::try_from(hosts.len() * ENTRY_LEN).expect("a short list");
    let mut packet = header(0x2B);
    packet.push(0x01);
    packet.extend_from_slice(&len.to_le_bytes());
    packet.extend_from_slice(&[0x00; 8]);
    for (i, host) in (1u64..).zip(hosts) {
        packet.extend_from_slice(&i.to_le_bytes());
        packet.extend_from_slice(&reversed(*host));
        packet.extend_from_slice(&[0x00; 3]);
    }
    packet
}

/// The answer to the notification request, as capture 4 of #71 has it: the
/// battery, the primary pod, 0x0C naming a bud - `pair` stands in for the one
/// AAP reached - 0x0E and 0x2E naming this host, the ear report and 0x2E
/// again.
fn report_packets(
    pair: Address,
    batteries: &[Battery; 3],
    ears: Ears,
) -> impl Iterator<Item = Vec<u8>> {
    [
        battery_packet(batteries),
        ears.primary_packet(false),
        in_use_packet(pair, false),
        host_packet(THIS_HOST),
        host_entry_packet(THIS_HOST),
        ear_packet(ears.report).to_vec(),
        host_entry_packet(THIS_HOST),
    ]
    .into_iter()
}

/// The second 0x0C, naming this host, and the startup dump after it, with
/// `mode`, the Conversation Awareness setting, the pair's name and its
/// Marketing Version in it - and with the setting on, level 09 after it,
/// unasked, as in the #150 run.
fn startup_dump(
    mode: NoiseMode,
    conversation_awareness: bool,
    name: &[u8],
    marketing_version: u32,
) -> Vec<Vec<u8>> {
    std::iter::once(in_use_packet(THIS_HOST, true))
        .chain(
            STARTUP_DUMP
                .iter()
                .map(|dumped| dumped.packet(mode, conversation_awareness, name, marketing_version)),
        )
        .chain(conversation_awareness.then(|| conversation::level_packet(LEVEL_IDLE).to_vec()))
        .collect()
}

/// Which `every` the pod on `side` is in at `now`, counting from 0: the left
/// pod's clock runs half of `every` off the right's.
fn period(side: PodSide, now: Duration, every: Duration) -> u64 {
    let offset = match side {
        PodSide::Right => Duration::ZERO,
        PodSide::Left => every / 2,
    };
    ((now + offset).as_millis() / every.as_millis()) as u64
}

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
/// (USB-C) count as behaving the same until a capture shows otherwise.
///
/// Where a default is faster or simpler than the hardware, its field or
/// constant says so. Those are:
///
/// - the address rotation, [`ADDRESS_ROTATION`], faster on purpose;
/// - the advertising rate, which is the world's: once a second from each pod
///   that advertises, where real pods advertise about 5 times a second;
/// - [`Quirks::handshake_delay`], [`Quirks::report_delay`] and
///   [`Quirks::dump_delay`], none or short where the hardware takes a second
///   or more;
/// - the closed case, which [`Pods::poll`] reports the moment the lid
///   closes, where the hardware took about 4.5 s;
/// - the payload, which changes only with the state, where real pods change
///   it again about 5 s, 30 s and 60 s after an event, and once about 2 s
///   after discovery starts - see [`Pods::advertisement`].
///
/// AirPods 4 answered a mode change otherwise (`docs/aap-noise-control.md`):
/// an echo on about one in four, no report of the mode an Off replaced, and
/// with Conversation Awareness on, as theirs was, level 09 on every change.
/// Their answers are `idle_level_only: true, echo_every: 4, stale_report:
/// None`, with [`Pods::set_conversation_awareness`] on.
///
/// Each one is read when it applies, so a change takes effect from the next
/// packet, advertisement or connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirks {
    /// Ignore a bare Off until `0x34 01` has allowed it: no mode change and
    /// nothing on the wire, where recent firmware on Pro 3 and Pro 2 plays an
    /// error chime.
    pub off_needs_allow: bool,
    /// With Conversation Awareness on, answer a mode change with level 09
    /// alone, at once, as AirPods 4 did (`docs/aap-noise-control.md`),
    /// rather than as a Pro 3 did: level 08 at once, the echo, and level 09
    /// after it. Off by default. With the setting off, neither sends a 0x4B.
    pub idle_level_only: bool,
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
    /// How long the pair takes to answer the handshake. A Pro 3 took 1.0-1.2 s
    /// in the #71 captures; none by default, as the app waits for the answer
    /// however long it takes, up to its handshake timeout. What the app sends
    /// before the answer is out is dropped, as the hardware drops it.
    pub handshake_delay: Duration,
    /// How long after the notification request the battery comes, with the
    /// rest of what answers it - the primary pod, the first 0x0C, the ear
    /// report - holding the state as it was when asked. None by default: a
    /// Pro 3 sent the battery 0.15-0.19 s after the request in the #71
    /// captures, and a pair seen in #62 took about a second, which the GNOME
    /// Settings battery went missing for.
    pub report_delay: Duration,
    /// How long after that the second 0x0C comes, naming this host, with the
    /// startup dump right after it: the mode, and the rest. Until then the app
    /// knows no mode. [`DUMP_DELAY`] by default, where a Pro 3 took 1.4-1.5 s.
    pub dump_delay: Duration,
}

impl Default for Quirks {
    fn default() -> Self {
        Self {
            off_needs_allow: true,
            idle_level_only: false,
            echo_every: 1,
            echo_delay: ECHO_DELAY,
            stale_report: Some(STALE_REPORT),
            silent_handshake: false,
            refuse_aap: 0,
            handshake_delay: Duration::ZERO,
            report_delay: Duration::ZERO,
            dump_delay: DUMP_DELAY,
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
    /// Drop the Bluetooth link, as the pods do once both are in the closed
    /// case: BlueZ reports the pair disconnected, and an AAP connection goes
    /// with the link. Nothing follows it, and the pair is disconnected from
    /// here on.
    Disconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pod {
    battery: Option<u8>,
    charging: bool,
    in_ear: bool,
    in_case: bool,
    /// Counts the times the pod came back from the closed case, each time
    /// under a new address.
    returns: u64,
}

/// The lid as the model last saw it, which a pod in the case times its
/// silence and its return by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LidSeen {
    open: bool,
    /// When the model first saw it so.
    since: Duration,
    /// Whether it opened on pods gone quiet in the closed case, which take a
    /// while to advertise again.
    on_silence: bool,
}

/// One AAP connection, from the app's connect to either side's hang-up.
#[derive(Debug, Clone)]
struct Session {
    /// Fixed when the connection opens: a quirk switched later does not turn a
    /// connection silent halfway.
    silent: bool,
    /// When the handshake's answer goes out, from the handshake on; `None`
    /// before. What comes before that is dropped.
    answered: Option<Duration>,
    /// The batteries as the app was last told, from the answer to the
    /// notification request on; `None` before. A change from them is what
    /// `poll` reports.
    batteries: Option<[Battery; 3]>,
    /// The mode as the app was last told, from the startup dump on; `None`
    /// before, when the pods report no change of it.
    mode: Option<NoiseMode>,
    /// Where the pods are and which is primary, as the app was last told,
    /// from the answer to the notification request on; `None` before. These
    /// come unasked.
    ears_reported: Option<Ears>,
    /// What waits for its time, in time order.
    scheduled: Vec<(Duration, Due)>,
}

/// Something a session has waiting for its time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Due {
    Act(Action),
    /// The answer to the notification request: the battery and what comes
    /// with it, with the state as it was when asked. From here on, `poll`
    /// reports what changed since.
    Report {
        batteries: [Battery; 3],
        ears: Ears,
    },
    /// The second 0x0C, naming this host, and the startup dump after it,
    /// carrying the mode as it is when it falls due.
    Dump,
    /// A stretch of speech switching the pods to Transparency.
    SpeechTransparency,
    /// A stretch of speech over, switching them back.
    SpeechBack,
}

/// A stretch of speech under way - see [`Pods::speak`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Speaking {
    /// When its last packet, level 09, is due.
    until: Duration,
    /// The mode to go back to once it is over, from the switch to
    /// Transparency on: the mode before it, or one set meanwhile.
    back_to: Option<NoiseMode>,
}

impl Session {
    /// Queues `due` for `at`, after anything already queued for that time.
    fn schedule(&mut self, at: Duration, due: Due) {
        let i = self.scheduled.partition_point(|(t, _)| *t <= at);
        self.scheduled.insert(i, (at, due));
    }

    /// Queues `packet` for `at`.
    fn send(&mut self, at: Duration, packet: Vec<u8>) {
        self.schedule(at, Due::Act(Action::Send(packet)));
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
    lid_seen: LidSeen,
    primary: PodSide,
    noise_mode: NoiseMode,
    allow_off: bool,
    conversation_awareness: bool,
    /// The pair's own name, as the pods hold it: bytes, as a rename cut at
    /// [`NAME_KEPT`] may leave a character half there.
    name: Vec<u8>,
    /// The firmware's Marketing Version, as the device information carries
    /// it in field 10.
    marketing_version: u32,
    /// A stretch of speech, while one is under way.
    speaking: Option<Speaking>,
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
    /// cancellation, Off not allowed, Conversation Awareness off, as in every
    /// #71 capture, named [`DEFAULT_NAME`], and connected to nothing.
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
            returns: 0,
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
            lid_seen: LidSeen {
                open: true,
                since: Duration::ZERO,
                on_silence: false,
            },
            primary: PodSide::Left,
            noise_mode: NoiseMode::NoiseCancellation,
            allow_off: false,
            conversation_awareness: false,
            name: DEFAULT_NAME.as_bytes().to_vec(),
            marketing_version: MARKETING_VERSION,
            speaking: None,
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

    /// Whether Conversation Awareness is on, the setting `0x28` writes.
    pub fn conversation_awareness(&self) -> bool {
        self.conversation_awareness
    }

    /// The pair's own name, as the pods hold it and send it in 0x1D.
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// The firmware's Marketing Version, as the pods send it in 0x1D.
    pub fn marketing_version(&self) -> u32 {
        self.marketing_version
    }

    /// Whether a stretch of speech is under way at `now` - see
    /// [`Pods::speak`].
    pub fn is_speaking(&self, now: Duration) -> bool {
        self.speaking.is_some_and(|s| now <= s.until)
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
    /// the ear. Over AAP, the next [`Pods::poll`] sends an ear report; and
    /// with the lid closed and the other pod in the case too, the pair drops
    /// its link.
    pub fn set_in_case(&mut self, side: PodSide, in_case: bool) {
        let pod = self.pod_mut(side);
        pod.in_case = in_case;
        pod.in_ear &= !in_case;
    }

    /// Opens or closes the case. The advertisement carries the lid only while
    /// the pod that sends it is in the case, or both are. With the sending pod
    /// worn and the other in the case, it says only that a pod is in the case,
    /// and the app keeps the lid it last heard.
    ///
    /// Over BLE, a pod in the closed case stops advertising [`LID_SILENCE`]
    /// after the lid closes, and comes back under a new address once it
    /// opens: the right pod [`RIGHT_RESUMES`] later, the left
    /// [`LEFT_RESUMES`] - see [`Pods::advertisements`]. The model times the
    /// lid from the next [`Pods::poll`] or [`Pods::advertisements`], which
    /// the world calls right after every change.
    ///
    /// Over AAP, a pod in the case is in the closed case at once - see
    /// [`Pods::poll`] - and with both pods in it, the pair drops its link at
    /// the next poll, as the pods do.
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
    /// such as a phone. A connected fake reports itself idle in byte 8 of its
    /// advertisements. Disconnecting takes an open AAP connection with it: the
    /// next [`Pods::poll`] hangs up.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// Changes the noise control mode from elsewhere - a stem press, or a
    /// phone. Over AAP, the next [`Pods::poll`] reports it. During a stretch
    /// of speech, it is also the mode the pods go back to after it.
    pub fn set_noise_mode(&mut self, mode: NoiseMode) {
        self.noise_mode = mode;
        if let Some(Speaking {
            back_to: Some(back_to),
            ..
        }) = &mut self.speaking
        {
            *back_to = mode;
        }
    }

    /// Turns Conversation Awareness on or off from elsewhere - a phone. Over
    /// AAP nothing reports it: LibrePods says the pods report the setting
    /// only in the startup dump, and whether a change on a phone reaches
    /// this host is a hardware check still to do (#150). The next
    /// connection's dump carries it.
    pub fn set_conversation_awareness(&mut self, enabled: bool) {
        self.conversation_awareness = enabled;
    }

    /// Names the pair, as it was named when paired, or from elsewhere: kept
    /// whole, whatever its length. Over AAP nothing reports it; the next
    /// connection's startup dump carries it.
    pub fn set_name(&mut self, name: &str) {
        self.name = name.as_bytes().to_vec();
    }

    /// Updates the firmware to the one whose Marketing Version is `raw`, as
    /// an update through a phone does. Over AAP nothing reports it; the next
    /// connection's startup dump carries it.
    pub fn set_marketing_version(&mut self, raw: u32) {
        self.marketing_version = raw;
    }

    /// The user speaks at `now` for `length`, and the pods hear it. With
    /// Conversation Awareness off they do nothing. With it on they send, as
    /// the first stretch of the #150 run went - levels and the mode switch at
    /// the real offsets, `SPEECH_*`:
    ///
    /// - level 01 at once, and a report of the mode they are in 20 ms later;
    /// - their switch to Transparency at 0.31 s, reported as a mode change;
    /// - level 02 at 0.82 s, while still speaking;
    /// - once `length` is over, 03, then 07 and 08 within 0.1 s;
    /// - their switch back at 0.46 s after that, reported, and level 09.
    ///
    /// Each part only if `length` leaves room for it before the end: a
    /// stretch shorter than 0.31 s never switches the mode. A mode set during
    /// the stretch takes effect at once, and is the one the pods go back to
    /// after it - the fake's choice, where the hardware's is not recorded.
    /// Mode changes are reported from the startup dump on, as always.
    ///
    /// Fails with no AAP connection open - the pods would speak to nobody -
    /// and while a stretch is under way.
    pub fn speak(&mut self, now: Duration, length: Duration) -> Result<(), String> {
        if self.is_speaking(now) {
            return Err("a stretch of speech is under way".into());
        }
        let mode = self.noise_mode;
        let Some(session) = self.session.as_mut().filter(|s| !s.silent) else {
            return Err("no AAP connection".into());
        };
        if !self.conversation_awareness {
            return Ok(());
        }
        let level = |level| conversation::level_packet(level).to_vec();
        let end = now + length;
        session.send(now, level(LEVEL_SPEAKING));
        if session.mode.is_some() {
            session.send(now + SPEECH_MODE_REPORT, noise::set_packet(mode).to_vec());
        }
        let switched = length > SPEECH_TRANSPARENCY;
        if switched {
            session.schedule(now + SPEECH_TRANSPARENCY, Due::SpeechTransparency);
        }
        if length > SPEECH_STILL {
            session.send(now + SPEECH_STILL, level(LEVEL_STILL_SPEAKING));
        }
        session.send(end, level(LEVEL_SPEECH_ENDED));
        for (after, ramp) in SPEECH_RAMP {
            session.send(end + after, level(ramp));
        }
        if switched {
            session.schedule(end + SPEECH_MODE_BACK, Due::SpeechBack);
        }
        session.send(end + SPEECH_IDLE, level(LEVEL_IDLE));
        self.speaking = Some(Speaking {
            until: end + SPEECH_IDLE,
            back_to: None,
        });
        Ok(())
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

    /// What the pair broadcasts at `now`: an advertisement from each pod that
    /// advertises then, the right pod's first. As in the #71 captures:
    ///
    /// - both pods advertise, each from an address of its own, while either
    ///   is in the case;
    /// - with both out of the case, worn or not, only the primary pod does,
    ///   the other going quiet the moment both are out and coming back as it
    ///   goes back in the case. Captures 1 and 2 saw the right pod alone, and
    ///   it was primary there. Each pod calls itself primary, and in session
    ///   1 of #142, both pods worn and the left one primary at times, the
    ///   advertisement's primary agreed with AAP's 13 times out of 13
    ///   (`docs/aap-ear-detection.md`): the pod advertising was the primary;
    /// - a pod in the closed case stops [`LID_SILENCE`] after the lid closes,
    ///   whether the other pod is in the case too or worn, which goes on;
    /// - when the lid opens on pods gone quiet, they come back under new
    ///   addresses, the right pod [`RIGHT_RESUMES`] later, the left
    ///   [`LEFT_RESUMES`].
    ///
    /// A lid opened before the pods went quiet keeps them going, at the
    /// addresses they had. No capture has that: it is the fake's choice.
    pub fn advertisements(&mut self, now: Duration) -> Vec<Advertisement> {
        self.see_lid(now);
        [PodSide::Right, PodSide::Left]
            .into_iter()
            .filter(|&side| self.advertises(side, now))
            .map(|side| self.advertisement(side, now))
            .collect()
    }

    /// Whether the pod on `side` advertises at `now`, by the lid as last seen
    /// - see [`Pods::advertisements`].
    fn advertises(&self, side: PodSide, now: Duration) -> bool {
        let (this, other) = self.pods(side);
        if !this.in_case {
            return other.in_case || side == self.primary;
        }
        let lid = self.lid_seen;
        if !lid.open {
            return now < lid.since + LID_SILENCE;
        }
        let resumes = match side {
            PodSide::Right => RIGHT_RESUMES,
            PodSide::Left => LEFT_RESUMES,
        };
        !lid.on_silence || now >= lid.since + resumes
    }

    /// Notes a change of the lid at `now`, the first time the model sees it.
    /// Opened on pods gone quiet in the closed case, each of them comes back
    /// under a new address, as both pods did in every #71 capture.
    fn see_lid(&mut self, now: Duration) {
        if self.lid_open == self.lid_seen.open {
            return;
        }
        let on_silence = self.lid_open && now >= self.lid_seen.since + LID_SILENCE;
        if on_silence {
            for pod in [&mut self.left, &mut self.right] {
                if pod.in_case {
                    pod.returns += 1;
                }
            }
        }
        self.lid_seen = LidSeen {
            open: self.lid_open,
            since: now,
            on_silence,
        };
    }

    /// The pod on `side`, and the other.
    fn pods(&self, side: PodSide) -> (&Pod, &Pod) {
        match side {
            PodSide::Left => (&self.left, &self.right),
            PodSide::Right => (&self.right, &self.left),
        }
    }

    /// What the pod on `side` sends at `now`, whether it advertises then or
    /// not.
    ///
    /// Each pod advertises as itself, as in the #71 captures: it calls itself
    /// primary - status bit 5 set by the left pod and clear from the right,
    /// its battery in the high nibble and first in the encrypted portion -
    /// and its status bit 6 says whether it is in the case. So each pod's
    /// advertisement decodes to the same left and right as the other's. The
    /// primary pod AAP reports decides only which pod advertises when one
    /// does - see [`Pods::advertisements`] - and, with both pods in the case,
    /// which one sets bit 6. In capture 2 only one pod set it then, the left
    /// before the pods were taken out and the right after they were put back:
    /// the fake simplifies that to the primary.
    ///
    /// The cleartext gives the batteries in 10% steps. The encrypted portion,
    /// under the pair's ENC_KEY, gives them exact, with the real address's
    /// last three bytes in bytes 7-9 - zeroed unless both pods are in the
    /// case, status bit 2, connected or not (#71) - and a tail, bytes 12-15,
    /// that follows from the rest of the payload, as a MIC would. Its byte 0
    /// is cleartext byte 8, the connection state, as in all 111 payloads of
    /// the second capture.
    ///
    /// So the payload is the state's, and changes as the state does and only
    /// then: a battery, charging, an ear, the case, the lid, a connection.
    /// Real AirPods repeat a payload for hundreds of advertisements - about 5
    /// a second from each pod - and change it when something happens. In the
    /// #71 captures and the hardware checks since, with the pods worn and
    /// connected to this host or to nothing, it changed again about 5 s, 30 s
    /// and 60 s after such a change with nothing else changing, and then
    /// stayed - 163 s in the longest stretch. Those gaps of 25 and 30 s are
    /// most likely the 20-30 s refresh once recorded here. On 2026-11-02 a
    /// pair idle in the open case, connected to a phone, sent the same
    /// payload for minutes (#153). The fake leaves out the three changes
    /// after an event, which carry nothing new to read: the app reads the
    /// state, and the state is the same.
    ///
    /// A pod out of the case while the other is in the closed case cannot
    /// reach the case, and sends its level as unknown, as the worn pod did in
    /// capture 4 of #71. Over AAP the case is away too then.
    pub fn advertisement(&self, side: PodSide, now: Duration) -> Advertisement {
        let (this, other) = self.pods(side);
        let both_in_case = self.left.in_case && self.right.in_case;
        let this_in_case = this.in_case && (!both_in_case || side == self.primary);
        let one_in_case = self.left.in_case || self.right.in_case;
        let connection_state = if self.connected {
            CONNECTION_IDLE
        } else {
            CONNECTION_NONE
        };
        let case_battery = self
            .case_battery
            .filter(|_| this.in_case || !other.in_case || self.lid_open);
        let cleartext = Cleartext {
            product_id: self.product_id,
            primary: side,
            this_in_case,
            both_in_case,
            one_in_case,
            left_battery: self.left.battery,
            right_battery: self.right.battery,
            case_battery,
            left_charging: self.left.charging,
            right_charging: self.right.charging,
            case_charging: self.case_charging,
            left_in_ear: self.left.in_ear,
            right_in_ear: self.right.in_ear,
            // With one pod in the case and one worn, the #71 captures have the
            // pod in the case report the lid and the worn one only status bit 4,
            // which leaves the app the lid it last heard.
            lid_open: (this_in_case || both_in_case).then_some(self.lid_open),
            color: self.color,
            connection_state,
        };

        let mac_suffix = if both_in_case {
            [self.mac[3], self.mac[4], self.mac[5]]
        } else {
            ZEROED_SUFFIX
        };
        let mut plaintext = Plaintext {
            header: connection_state,
            primary_battery: this.battery,
            primary_charging: this.charging,
            secondary_battery: other.battery,
            secondary_charging: other.charging,
            case_battery,
            case_charging: self.case_charging,
            mac_suffix,
            tail: [0; 4],
        };
        let cleartext = cleartext.to_bytes();
        plaintext.tail = self.tail(side, &cleartext, &plaintext.to_bytes());
        let encrypted = encrypt(&plaintext.to_bytes(), &self.keys.enc_key);

        Advertisement {
            address: self.address_at(side, now),
            data: proximity_message(&cleartext, &encrypted),
        }
    }

    /// Bytes 12-15 of the plaintext the pod on `side` sends with `cleartext`,
    /// `head` the plaintext without them. On the hardware they change
    /// whenever the payload does, and may be a MIC (#71); here they follow
    /// from the rest of the payload, as a MIC would, so they change with it
    /// and only with it.
    fn tail(&self, side: PodSide, cleartext: &[u8], head: &[u8; 16]) -> [u8; 4] {
        let mut block = encrypt(head, &self.keys.irk);
        for (byte, clear) in block.iter_mut().zip(cleartext) {
            *byte ^= clear;
        }
        block[15] ^= u8::from(side == PodSide::Right);
        let mac = encrypt(&block, &self.keys.irk);
        [mac[0], mac[1], mac[2], mac[3]]
    }

    /// The random address the pod on `side` advertises from at `now`. It
    /// changes every [`ADDRESS_ROTATION`], on the pod's own clock, as the pod
    /// comes back from the closed case, and with the keys.
    ///
    /// A resolvable private address, as real AirPods advertise from: its hash
    /// is `ah` of its `prand` under the pair's IRK, so the IRK the key packet
    /// carries resolves it as it resolves the hardware's (#134).
    pub fn address_at(&self, side: PodSide, now: Duration) -> Address {
        let bytes = self.derive(b'A', side, period(side, now, ADDRESS_ROTATION));
        resolvable_address(&self.keys.irk, [bytes[0], bytes[1], bytes[2]])
    }

    /// The app opened an AAP connection at `now`. There is no AAP without a
    /// Bluetooth link, so the pair is connected from here on.
    pub fn aap_opened(&mut self, now: Duration) {
        self.connected = true;
        let mut session = Session {
            silent: self.quirks.silent_handshake,
            answered: None,
            batteries: None,
            mode: None,
            ears_reported: None,
            scheduled: Vec::new(),
        };
        if session.silent {
            session.schedule(now + SILENT_HANG_UP, Due::Act(Action::HangUp));
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
        self.end_session();
    }

    /// Ends the AAP connection, and what was due on it. A stretch of speech
    /// still under way ends with it, the pods back in the mode they were
    /// switched from: the switch back was due on the connection.
    fn end_session(&mut self) {
        self.session = None;
        if let Some(back_to) = self.speaking.take().and_then(|s| s.back_to) {
            self.noise_mode = back_to;
        }
    }

    /// A packet the app sent at `now`. Whatever it calls for comes out of the
    /// next [`Pods::poll`].
    ///
    /// Requests are known by their command byte, and settings by their
    /// sub-command too. What the link brings up comes in the order of capture
    /// 4 of #71:
    ///
    /// - the handshake gets its answer, all 18 bytes, after
    ///   [`Quirks::handshake_delay`], and 0x2B, the hosts the pair knows, with
    ///   it, unasked. Until the answer is out, everything else is dropped, as
    ///   the hardware drops it;
    /// - the features packet (0x4D) gets no answer;
    /// - the notification request (0x0F) the battery, the primary pod (0x08),
    ///   0x0C naming a bud - the pair itself - then 0x0E and 0x2E naming this
    ///   host, the ear report (0x06) and 0x2E again, after
    ///   [`Quirks::report_delay`]; from then on a report of every change to
    ///   the batteries and the pods' places. After [`Quirks::dump_delay`] a
    ///   second 0x0C names this host, and the startup dump follows it - the
    ///   mode among some twenty other settings - and from then on a report of
    ///   every change of mode. See [`THIS_HOST`];
    /// - the key request (0x30) the key packet, IRK then ENC_KEY;
    ///
    /// and on a link that is up:
    ///
    /// - a mode change its delayed echo, and an Off a report of the mode it
    ///   replaced ahead of that, as a Pro 3 answers; or nothing, for an Off not
    ///   allowed - see [`Quirks`], which switch to AirPods 4's answers too.
    ///   With Conversation Awareness on, levels 08 and 09 come around the
    ///   echo, the refused Off's too, as a Pro 3 sent them (#150);
    /// - `0x34` sets allow-Off, and `0x28` Conversation Awareness. What
    ///   either answers is not recorded, so the fake answers nothing.
    /// - a rename (0x1A) the pair's name, answering nothing, as a Pro 3
    ///   answered nothing (#160); the next connection's 0x1D carries it.
    ///
    /// Anything else is recorded and otherwise ignored, as is everything
    /// arriving with no connection open or on a silent one.
    pub fn receive(&mut self, packet: &[u8], now: Duration) {
        self.received.push(packet.to_vec());
        let Some(session) = self.session.as_ref().filter(|s| !s.silent) else {
            return;
        };
        let opcode = packet.get(OPCODE).copied();
        if opcode != Some(OP_HANDSHAKE) && !session.answered.is_some_and(|at| at <= now) {
            // Ahead of the handshake's answer.
            return;
        }
        match opcode {
            Some(OP_HANDSHAKE) => self.answer_handshake(now),
            Some(OP_NOTIFICATIONS) => self.start_notifications(now),
            Some(OP_KEY_REQUEST) => self.send(now, self.key_packet()),
            Some(name::RENAME_OPCODE) => self.rename(packet),
            _ => self.settings_command(packet, now),
        }
    }

    /// What is due on the AAP connection by `now`, in order: replies and timed
    /// actions as they fall due, then a report of each change the app has not
    /// been told of - the pods' places, then batteries and charging in one
    /// battery packet, then the mode in a 0x0D report. Taken once; a second
    /// call at the same time returns nothing new.
    ///
    /// A new primary pod comes as the hardware's does: the ear report in the
    /// new order now, then two primary pod packets, the first
    /// [`PRIMARY_AFTER_EAR`] later and marked as a switch, the second
    /// [`SECOND_PRIMARY`] after that. A pod in the case is in the closed case
    /// the moment the lid closes, on purpose, where the hardware took about
    /// 4.5 s: the primary pod packet says the secondary cannot be reached,
    /// then the ear report has it in the closed case, then the battery has it
    /// and the case Disconnected, as capture 4 of #71 has them, and the same
    /// three as the lid opens.
    ///
    /// A [`Action::HangUp`] ends the connection, and nothing comes after it.
    /// With both pods in the closed case the pair drops its link, AAP or not:
    /// [`Action::Disconnect`], alone.
    pub fn poll(&mut self, now: Duration) -> Vec<Action> {
        self.see_lid(now);
        if self.connected && self.left.in_case && self.right.in_case && !self.lid_open {
            // Both pods in the closed case: the pods drop the link (#142).
            self.connected = false;
            self.end_session();
            return vec![Action::Disconnect];
        }
        let batteries = self.aap_batteries();
        let ears = self.ears();
        let mac = self.mac;
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        if !self.connected {
            // The link went down under the connection, and takes it along.
            self.end_session();
            return vec![Action::HangUp];
        }

        let due = session.scheduled.partition_point(|(at, _)| *at <= now);
        let mut actions = Vec::new();
        for (_, due) in session.scheduled.drain(..due).collect::<Vec<_>>() {
            // The mode a stretch of speech switches to, if this is one.
            let mut switched = None;
            match due {
                Due::Act(Action::HangUp) => {
                    actions.push(Action::HangUp);
                    self.end_session();
                    return actions;
                }
                Due::Act(action) => actions.push(action),
                Due::Report {
                    batteries: asked,
                    ears: placed,
                } => {
                    actions.extend(report_packets(mac, &asked, placed).map(Action::Send));
                    session.batteries = Some(asked);
                    session.ears_reported = Some(placed);
                }
                Due::Dump => {
                    let mode = self.noise_mode;
                    let dump = startup_dump(
                        mode,
                        self.conversation_awareness,
                        &self.name,
                        self.marketing_version,
                    );
                    actions.extend(dump.into_iter().map(Action::Send));
                    session.mode = Some(mode);
                }
                Due::SpeechTransparency => {
                    if let Some(speaking) = &mut self.speaking {
                        speaking.back_to = Some(self.noise_mode);
                        switched = Some(NoiseMode::Transparency);
                    }
                }
                Due::SpeechBack => {
                    switched = self.speaking.as_mut().and_then(|s| s.back_to.take());
                }
            }
            // Reported in its place among what falls due, where the report
            // of a change below would come after all of it.
            if let Some(mode) = switched {
                self.noise_mode = mode;
                if let Some(reported) = &mut session.mode
                    && *reported != mode
                {
                    actions.push(Action::Send(noise::set_packet(mode).to_vec()));
                    *reported = mode;
                }
            }
        }
        let mode = self.noise_mode;

        // Where the pods are, first: the hardware sends the battery after it.
        if let Some(reported) = &mut session.ears_reported
            && *reported != ears
        {
            let reported = std::mem::replace(reported, ears);
            if reported.primary != ears.primary {
                // A role switch: the ear report in the new order first.
                if reported.report != ears.report {
                    actions.push(Action::Send(ear_packet(ears.report).to_vec()));
                }
                let first = now + PRIMARY_AFTER_EAR;
                session.send(first, ears.primary_packet(true));
                session.send(first + SECOND_PRIMARY, ears.primary_packet(false));
            } else {
                // The lid closing or opening on a pod: the primary pod's say
                // on the secondary first.
                if reported.secondary_reachable() != ears.secondary_reachable() {
                    actions.push(Action::Send(ears.primary_packet(false)));
                }
                if reported.report != ears.report {
                    actions.push(Action::Send(ear_packet(ears.report).to_vec()));
                }
            }
        }
        if let Some(reported) = &mut session.batteries
            && *reported != batteries
        {
            actions.push(Action::Send(battery_packet(&batteries)));
            *reported = batteries;
        }
        if let Some(reported) = &mut session.mode
            && *reported != mode
        {
            actions.push(Action::Send(noise::set_packet(mode).to_vec()));
            *reported = mode;
        }
        actions
    }

    /// When the next timed action falls due, for the caller to [`Pods::poll`]
    /// at: the handshake's answer, the answer to the notification request,
    /// the startup dump, an echo or a stale report of a mode, a speech level
    /// or a stretch of speech's switch of mode, a role switch's primary pod
    /// packets, or the silent pair's hang-up. `None` with nothing
    /// scheduled. Replies due at once - the key packet, say - are counted
    /// too; changes made through the setters are not: poll after each.
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

    /// The handshake's answer, after [`Quirks::handshake_delay`], and the
    /// hosts the pair knows with it, unasked: 0x2B came 3 µs after the answer
    /// in one #71 capture, before the app had sent anything it might answer,
    /// and 30 ms after it in another.
    fn answer_handshake(&mut self, now: Duration) {
        let at = now + self.quirks.handshake_delay;
        let hosts = known_hosts_packet(&[self.mac, THIS_HOST, ANOTHER_HOST]);
        if let Some(session) = &mut self.session {
            session.answered.get_or_insert(at);
            session.send(at, HANDSHAKE_ACK.to_vec());
            session.send(at, hosts);
        }
    }

    fn pod_mut(&mut self, side: PodSide) -> &mut Pod {
        match side {
            PodSide::Left => &mut self.left,
            PodSide::Right => &mut self.right,
        }
    }

    /// Sixteen bytes that look random but follow from the IRK, a purpose, a
    /// pod and a number, so the rotating addresses are a function of state
    /// and time like everything else here.
    fn derive(&self, purpose: u8, side: PodSide, n: u64) -> [u8; 16] {
        let mut block = [0u8; 16];
        block[0] = purpose;
        block[1] = u8::from(side == PodSide::Right);
        // The address changes as the pod comes back from the closed case.
        let (this, _) = self.pods(side);
        block[2..8].copy_from_slice(&this.returns.to_be_bytes()[2..]);
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
            session.send(at, packet);
        }
    }

    /// The answer to the notification request, as capture 4 of #71 has it:
    /// the battery, the primary pod, 0x0C naming a bud, 0x0E and 0x2E naming
    /// this host, the ear report and 0x2E again; then, after the dump delay,
    /// the second 0x0C and the startup dump. From the battery on, the app
    /// hears of every change to the batteries and the pods' places, and from
    /// the dump on, of every change of mode.
    ///
    /// The answer holds the state as it was when asked; what changes before
    /// it is due follows it in the same poll, so the last word is the state
    /// of the moment.
    fn start_notifications(&mut self, now: Duration) {
        let report = Due::Report {
            batteries: self.aap_batteries(),
            ears: self.ears(),
        };
        let at = now + self.quirks.report_delay;
        let dump_at = at + self.quirks.dump_delay;
        if let Some(session) = &mut self.session {
            session.schedule(at, report);
            session.schedule(dump_at, Due::Dump);
        }
    }

    /// A rename, `1A 00 01 [length] 00 [name]` after the header: the pods
    /// take the name at once and answer nothing, as a Pro 3 did in #160. A
    /// name over [`NAME_KEPT`] bytes is cut to its first 127, as a Pro 3 cut
    /// it, inside a character if one falls there. What the pods do with a
    /// packet shaped otherwise, or an empty name, is not recorded; the fake
    /// ignores either.
    fn rename(&mut self, packet: &[u8]) {
        let Some(&[0x00, 0x01, len, 0x00]) = packet.get(OPCODE + 1..OPCODE + 5) else {
            return;
        };
        let Some(given) = packet.get(OPCODE + 5..) else {
            return;
        };
        if given.len() != usize::from(len) || given.is_empty() {
            return;
        }
        self.name = given[..given.len().min(NAME_KEPT)].to_vec();
    }

    /// A command in the 0x09 settings family.
    fn settings_command(&mut self, packet: &[u8], now: Duration) {
        if let Some(allowed) = noise::parse_allow_off_packet(packet) {
            self.allow_off = allowed;
            return;
        }
        if let Ok(enabled) = conversation::parse_setting_packet(packet) {
            // What it answers is not recorded; LibrePods says the setting
            // is not reported again, so the fake answers nothing.
            self.conversation_awareness = enabled;
            return;
        }
        let Ok(mode) = noise::parse_noise_mode_packet(packet) else {
            return;
        };
        let levels = self.conversation_awareness && !self.quirks.idle_level_only;
        if mode == NoiseMode::Off && !self.allow_off && self.quirks.off_needs_allow {
            // The error chime: the mode stays. With Conversation Awareness
            // on, a Pro 3 framed even this one in levels 08 and 09 around a
            // report of the mode it stayed in (#150); otherwise nothing.
            let (current, delay) = (self.noise_mode, self.quirks.echo_delay);
            if levels && let Some(session) = &mut self.session {
                let level = |level| conversation::level_packet(level).to_vec();
                session.send(now, level(LEVEL_NORMAL));
                session.send(now + delay, noise::set_packet(current).to_vec());
                session.send(now + delay, level(LEVEL_IDLE));
            }
            return;
        }

        let previous = std::mem::replace(&mut self.noise_mode, mode);
        self.mode_changes += 1;
        if let Some(Speaking {
            back_to: Some(back_to),
            ..
        }) = &mut self.speaking
        {
            // In Transparency for speech: this is the mode to go back to.
            *back_to = mode;
        }
        let Some(session) = &mut self.session else {
            return;
        };
        // The app knows what it asked for; only a change from elsewhere is
        // news to it. Before the startup dump, the dump tells it.
        if let Some(reported) = &mut session.mode {
            *reported = mode;
        }
        let level = |level| conversation::level_packet(level).to_vec();
        if self.conversation_awareness {
            // A Pro 3's 08 now and 09 after the echo, or AirPods 4's 09.
            let first = if levels { LEVEL_NORMAL } else { LEVEL_IDLE };
            session.send(now, level(first));
        }
        // Stale, so not what the app was told: `session.mode` keeps the mode
        // set, and only the echo corrects the report.
        if let Some(delay) = self.quirks.stale_report
            && mode == NoiseMode::Off
            && previous != NoiseMode::Off
        {
            session.send(now + delay, noise::set_packet(previous).to_vec());
        }
        // Carries the mode this command set, even if another has replaced it
        // by the time it arrives: a stale report, like those in run 1 of
        // docs/aap-noise-control.md.
        if self.quirks.echo_every != 0 && self.mode_changes.is_multiple_of(self.quirks.echo_every) {
            session.send(
                now + self.quirks.echo_delay,
                noise::set_packet(mode).to_vec(),
            );
        }
        // After the echo, whether one comes for this change or not.
        if levels {
            session.send(now + self.quirks.echo_delay, level(LEVEL_IDLE));
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

    /// What the pod on `side` sends at `now`, parsed and decrypted as the app
    /// does it.
    fn read(pods: &Pods, side: PodSide, now: Duration) -> ProximityData {
        let ad = pods.advertisement(side, now);
        let mut pd = parse_proximity_data(&ad.data).unwrap();
        let encrypted = pd.encrypted_portion().unwrap().to_vec();
        let decrypted = decrypt_for_device(&encrypted, &pods.keys().enc_key, pods.mac()).unwrap();
        pd.add_decrypted_data(&decrypted).unwrap();
        pd
    }

    /// The plaintext of what the pod on `side` sends at `now`, unchecked.
    fn plaintext(pods: &Pods, side: PodSide, now: Duration) -> [u8; 16] {
        let pd = parse_proximity_data(&pods.advertisement(side, now).data).unwrap();
        decrypt_raw(pd.encrypted_portion().unwrap(), &pods.keys().enc_key).unwrap()
    }

    /// The pods that advertise at `now`, by the addresses they advertise
    /// from.
    fn advertising(pods: &mut Pods, now: Duration) -> Vec<PodSide> {
        pods.advertisements(now)
            .iter()
            .map(|ad| {
                [PodSide::Right, PodSide::Left]
                    .into_iter()
                    .find(|&side| pods.address_at(side, now) == ad.address)
                    .expect("a pod's address")
            })
            .collect()
    }

    /// The packets among `actions`.
    fn sent(actions: &[Action]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .map(|a| match a {
                Action::Send(p) => p.clone(),
                Action::HangUp | Action::Disconnect => panic!("hung up among {actions:?}"),
            })
            .collect()
    }

    /// A Pro 3 pair with an AAP connection the app has run its connect
    /// sequence on, the replies taken, the startup dump with them.
    fn subscribed() -> Pods {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.dump_delay = Duration::ZERO;
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

        assert_eq!(advertising(&mut pods, ms(0)), [PodSide::Left], "both out");
        let ad = pods.advertisement(PodSide::Right, ms(0));
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

        let exact = read(&pods, PodSide::Right, ms(0));
        assert_eq!(
            (exact.left_battery, exact.right_battery),
            (Some(83), Some(41))
        );
        assert_eq!(exact.case_battery, None);
        assert!(exact.left_charging);
    }

    /// Each pod advertises from an address of its own, as itself, calling
    /// itself primary - the right pod's status `0x55` beside the left's `0x35`
    /// in the #71 captures - whichever pod AAP has as primary. Both decode to
    /// the same left and right. Only the pod in the case reads the lid; the
    /// worn one says a pod is in the case.
    #[test]
    fn each_pod_advertises_as_itself() {
        let mut pods = Pods::new(PRO_2_USB_C, MAC);
        pods.set_primary(PodSide::Left);
        pods.set_in_ear(PodSide::Right, true);
        pods.set_battery(Part::Right, Some(12));
        pods.set_charging(Part::Right, true);

        assert_eq!(
            advertising(&mut pods, ms(0)),
            [PodSide::Right, PodSide::Left]
        );
        let right = read(&pods, PodSide::Right, ms(0));
        let left = read(&pods, PodSide::Left, ms(0));
        assert_eq!(right.primary_pod(), PodSide::Right);
        assert_eq!(left.primary_pod(), PodSide::Left);
        for pd in [&right, &left] {
            assert_eq!((pd.left_battery, pd.right_battery), (Some(80), Some(12)));
            assert!(pd.right_charging && !pd.left_charging);
            assert!(pd.right_in_ear && !pd.left_in_ear);
            assert!(pd.one_in_case);
        }
        assert_eq!(left.status & 0x40, 0x40, "the left pod in the case");
        assert_eq!(right.status & 0x40, 0, "the right pod out of it");
        assert_eq!(left.lid_open, Some(true));
        assert_eq!(right.lid_open, None, "the right pod, worn");
    }

    /// Wherever each pod is - in an ear, in the case or out - both pods'
    /// advertisements decode to the pods as they are: the flip of each and
    /// the ear swap undo each other.
    #[test]
    fn both_pods_decode_to_the_same_left_and_right() {
        // In an ear, in the case.
        let places = [(true, false), (false, true), (false, false)];
        for left_place in places {
            for right_place in places {
                let mut pods = Pods::new(PRO_3, MAC);
                pods.set_battery(Part::Left, Some(83));
                pods.set_battery(Part::Right, Some(41));
                pods.set_charging(Part::Left, true);
                for (side, (in_ear, in_case)) in
                    [(PodSide::Left, left_place), (PodSide::Right, right_place)]
                {
                    pods.set_in_case(side, in_case);
                    pods.set_in_ear(side, in_ear);
                }
                for side in [PodSide::Left, PodSide::Right] {
                    let pd = read(&pods, side, ms(0));
                    let what = format!("{side:?}'s, left {left_place:?}, right {right_place:?}");
                    assert_eq!(
                        (pd.left_battery, pd.right_battery),
                        (Some(83), Some(41)),
                        "{what}"
                    );
                    assert!(pd.left_charging && !pd.right_charging, "{what}");
                    assert_eq!(
                        (pd.left_in_ear, pd.right_in_ear),
                        (left_place.0, right_place.0),
                        "{what}"
                    );
                    assert_eq!(pd.one_in_case, left_place.1 || right_place.1, "{what}");
                }
            }
        }
    }

    /// Byte 6 as capture 4 of #71 has it: `0x11` from a pod out of the case,
    /// whether the case charges or not; `0x31` from one in a case that does
    /// not, `0x51` from one in a case that does; bit 3 for a closed lid.
    #[test]
    fn byte_6_follows_the_pod_and_the_case_charging() {
        // The type and length, then bytes 0-6 of the payload.
        let byte_6 = |pods: &Pods, side| pods.advertisement(side, ms(0)).data[8];
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_in_ear(PodSide::Left, true);
        for (charging, in_case) in [(false, 0x31), (true, 0x51)] {
            pods.set_charging(Part::Case, charging);
            pods.set_lid(true);
            assert_eq!(byte_6(&pods, PodSide::Right), in_case, "{charging}");
            assert_eq!(byte_6(&pods, PodSide::Left), 0x11, "{charging}: worn");
            pods.set_lid(false);
            assert_eq!(byte_6(&pods, PodSide::Right), in_case | 0x08);
            assert_eq!(byte_6(&pods, PodSide::Left), 0x11);
        }
    }

    /// Pods in the closed case go quiet [`LID_SILENCE`] after it closes - from
    /// when the model saw it close - and once it opens they come back under
    /// new addresses, the right pod after [`RIGHT_RESUMES`], the left after
    /// [`LEFT_RESUMES`], as in the #71 captures.
    #[test]
    fn the_closed_case_silences_the_pods_until_it_opens() {
        let both = [PodSide::Right, PodSide::Left];
        let mut pods = Pods::new(PRO_3, MAC);
        let addresses = |pods: &Pods| both.map(|side| pods.address_at(side, ms(0)));
        let before = addresses(&pods);

        let closed = ms(1000);
        pods.set_lid(false);
        assert_eq!(advertising(&mut pods, closed), both, "a while yet");
        let last = closed + LID_SILENCE - ms(1);
        assert_eq!(advertising(&mut pods, last), both);
        let lid = read(&pods, PodSide::Left, last).lid_open;
        assert_eq!(lid, Some(false), "the lid closed, told");
        assert!(advertising(&mut pods, closed + LID_SILENCE).is_empty());
        assert!(advertising(&mut pods, ms(60_000)).is_empty());

        let opened = ms(60_000);
        pods.set_lid(true);
        assert!(advertising(&mut pods, opened).is_empty(), "not at once");
        let right = opened + RIGHT_RESUMES;
        assert!(advertising(&mut pods, right - ms(1)).is_empty());
        assert_eq!(advertising(&mut pods, right), [PodSide::Right]);
        let left = opened + LEFT_RESUMES;
        assert_eq!(advertising(&mut pods, left - ms(1)), [PodSide::Right]);
        assert_eq!(advertising(&mut pods, left), both);

        let after = addresses(&pods);
        for (before, after) in before.iter().zip(&after) {
            assert_ne!(before, after, "a new address");
            assert!(resolves(&pods.keys().irk, *after));
        }
    }

    /// A lid closed and opened again before the pods went quiet keeps them
    /// going, at the addresses they had.
    #[test]
    fn a_lid_opened_before_the_silence_keeps_the_pods_going() {
        let both = [PodSide::Right, PodSide::Left];
        let mut pods = Pods::new(PRO_3, MAC);
        let before = both.map(|side| pods.address_at(side, ms(0)));
        pods.set_lid(false);
        assert_eq!(pods.poll(ms(1000)), [], "seen, by the poll");
        pods.set_lid(true);
        let opened = ms(1000) + LID_SILENCE - ms(1);
        assert_eq!(advertising(&mut pods, opened), both);
        assert_eq!(both.map(|side| pods.address_at(side, ms(0))), before);
    }

    /// One pod in the closed case and the other worn, as in capture 4 of #71:
    /// the pod in the case goes quiet, the worn one goes on, its case level
    /// unknown. Opened, the pod in the case comes back.
    #[test]
    fn a_pod_in_the_closed_case_goes_quiet_beside_a_worn_one() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_in_ear(PodSide::Left, true);
        let closed = ms(1000);
        pods.set_lid(false);
        let both = [PodSide::Right, PodSide::Left];
        assert_eq!(advertising(&mut pods, closed), both);
        let worn = read(&pods, PodSide::Left, closed);
        assert_eq!((worn.lid_open, worn.one_in_case), (None, true));
        assert_eq!(worn.case_battery, None, "the case out of reach");
        let in_case = read(&pods, PodSide::Right, closed);
        assert_eq!(in_case.lid_open, Some(false));
        assert_eq!(in_case.case_battery, Some(50));

        let quiet = closed + LID_SILENCE;
        assert_eq!(advertising(&mut pods, quiet), [PodSide::Left]);
        let opened = ms(30_000);
        pods.set_lid(true);
        assert_eq!(advertising(&mut pods, opened), [PodSide::Left]);
        let back = opened + RIGHT_RESUMES;
        assert_eq!(advertising(&mut pods, back), both);
        assert_eq!(read(&pods, PodSide::Left, back).case_battery, Some(50));
    }

    /// Both pods advertise while one is in the case; with both out, worn or
    /// not, the primary pod alone - the right one in captures 1 and 2 of #71,
    /// where it was primary - and the other after a role switch.
    #[test]
    fn only_the_primary_pod_advertises_with_both_out_of_the_case() {
        let both = [PodSide::Right, PodSide::Left];
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_primary(PodSide::Right);
        assert_eq!(advertising(&mut pods, ms(0)), both, "both in the case");
        pods.set_in_ear(PodSide::Left, true);
        assert_eq!(advertising(&mut pods, ms(0)), both, "one worn");
        pods.set_in_ear(PodSide::Right, true);
        assert_eq!(advertising(&mut pods, ms(0)), [PodSide::Right], "both worn");
        pods.set_primary(PodSide::Left);
        assert_eq!(advertising(&mut pods, ms(0)), [PodSide::Left], "switched");
        pods.set_in_ear(PodSide::Left, false);
        assert_eq!(advertising(&mut pods, ms(0)), [PodSide::Left], "one out");
        pods.set_in_ear(PodSide::Right, false);
        assert_eq!(advertising(&mut pods, ms(0)), [PodSide::Left], "both out");
        pods.set_in_case(PodSide::Right, true);
        assert_eq!(advertising(&mut pods, ms(0)), both, "the right one back");
    }

    /// With both pods in the case, only one sets status bit 6, as in capture
    /// 2 of #71: in the fake, the primary.
    #[test]
    fn with_both_pods_in_the_case_the_primary_alone_says_it_is_in_it() {
        let mut pods = Pods::new(PRO_3, MAC);
        let bit_6 = |pods: &Pods, side| read(pods, side, ms(0)).status & 0x40 != 0;
        for primary in [PodSide::Left, PodSide::Right] {
            pods.set_primary(primary);
            for side in [PodSide::Left, PodSide::Right] {
                assert_eq!(bit_6(&pods, side), side == primary, "{side:?}");
                assert_eq!(read(&pods, side, ms(0)).lid_open, Some(true));
            }
        }
    }

    #[test]
    fn advertises_the_lid_while_the_sending_pod_is_in_the_case() {
        let mut pods = Pods::new(PRO_3, MAC);
        let lid = |pods: &Pods, side| read(pods, side, ms(0)).lid_open;
        pods.set_lid(false);
        assert_eq!(lid(&pods, PodSide::Left), Some(false));
        assert_eq!(lid(&pods, PodSide::Right), Some(false));
        pods.set_lid(true);
        assert_eq!(lid(&pods, PodSide::Left), Some(true));
        assert_eq!(lid(&pods, PodSide::Right), Some(true));
        pods.set_in_ear(PodSide::Right, true);
        assert_eq!(lid(&pods, PodSide::Left), Some(true), "in the case");
        assert_eq!(lid(&pods, PodSide::Right), None, "worn");
        pods.set_in_ear(PodSide::Left, true);
        assert_eq!(lid(&pods, PodSide::Right), None);
    }

    /// With both pods in the case - status bit 2 - bytes 7-9 carry the real
    /// address's suffix; with either out, zeros. Connected or not, as in the
    /// #71 captures. Either way the pair's key decrypts it and
    /// `matches_device` takes it, and byte 0 is the connection state.
    #[test]
    fn the_suffix_is_there_while_both_pods_are_in_the_case() {
        for connected in [false, true] {
            let mut pods = Pods::new(PRO_3, MAC);
            pods.set_connected(connected);
            let state = if connected {
                CONNECTION_IDLE
            } else {
                CONNECTION_NONE
            };
            for side in [PodSide::Left, PodSide::Right] {
                let plain = plaintext(&pods, side, ms(0));
                assert_eq!(plain[7..10], [0xDD, 0xEE, 0xFF], "{side:?}, {state}");
                assert!(matches_device(&plain, MAC));
                assert_eq!(plain[0], state, "byte 0 follows byte 8");
                assert_eq!(read(&pods, side, ms(0)).connection_state, state);
            }

            pods.set_in_ear(PodSide::Left, true);
            for side in [PodSide::Left, PodSide::Right] {
                let plain = plaintext(&pods, side, ms(0));
                assert_eq!(plain[7..10], ZEROED_SUFFIX, "{side:?}, {state}");
                assert!(matches_device(&plain, MAC));
            }
        }
    }

    /// A wrong key passes by chance one time in 2^23 - the odds the app works
    /// with too, and the only way this test can fail spuriously.
    #[test]
    fn a_different_key_fails() {
        for in_case in [false, true] {
            let mut pods = Pods::new(PRO_3, MAC);
            pods.set_in_case(PodSide::Left, in_case);
            let pd = parse_proximity_data(&pods.advertisement(PodSide::Left, ms(0)).data).unwrap();
            let encrypted = pd.encrypted_portion().unwrap();
            let other = Keys::generate();
            assert!(decrypt_for_device(encrypted, &other.enc_key, MAC).is_err());
            // The IRK is a key of the pair's, but not this one.
            assert!(decrypt_for_device(encrypted, &pods.keys().irk, MAC).is_err());
        }
    }

    /// Each pod has an address of its own, which rotates on its own clock:
    /// the left pod's half a rotation in, the right pod's at a whole. Every
    /// address resolves with the IRK the key packet carries, as the
    /// hardware's do, and with no other pair's.
    #[test]
    fn each_pods_address_rotates_and_resolves_with_the_irk() {
        let pods = Pods::new(PRO_3, MAC);
        let half = ADDRESS_ROTATION / 2;
        let right = pods.address_at(PodSide::Right, ms(0));
        let left = pods.address_at(PodSide::Left, ms(0));
        assert_ne!(right, left, "an address each");

        assert_eq!(pods.address_at(PodSide::Left, half - ms(1)), left);
        let left_next = pods.address_at(PodSide::Left, half);
        assert_ne!(left_next, left);
        assert_eq!(pods.address_at(PodSide::Right, half), right);

        assert_eq!(
            pods.address_at(PodSide::Right, ADDRESS_ROTATION - ms(1)),
            right
        );
        let right_next = pods.address_at(PodSide::Right, ADDRESS_ROTATION);
        assert_ne!(right_next, right);
        assert_eq!(pods.address_at(PodSide::Left, ADDRESS_ROTATION), left_next);
        let ad = pods.advertisement(PodSide::Right, ADDRESS_ROTATION);
        assert_eq!(ad.address, right_next);

        let other = Pods::new(PRO_3, MAC);
        for address in [right, left, right_next, left_next] {
            assert_ne!(address, MAC);
            assert!(resolves(&pods.keys().irk, address), "{address}");
            assert!(!resolves(&other.keys().irk, address), "{address}");
        }
        let others = other.address_at(PodSide::Right, ms(0));
        assert_ne!(others, right, "every pair has its own");
    }

    /// A pod repeats its payload for as long as nothing changes - for
    /// minutes, as a pair idle in the open case did on 2026-11-02 (#153) -
    /// whatever address it sends it from. A change of state changes it at
    /// once, and the tail - bytes 12-15 of the plaintext - with it, even a
    /// change only the cleartext carries.
    #[test]
    fn the_payload_changes_with_the_state_and_only_then() {
        let mut pods = Pods::new(PRO_3, MAC);
        let data = |pods: &Pods, side, now| pods.advertisement(side, now).data;
        let later = Duration::from_secs(600);
        assert_ne!(
            pods.address_at(PodSide::Right, later),
            pods.address_at(PodSide::Right, ms(0)),
            "rotated meanwhile"
        );
        let first = data(&pods, PodSide::Right, ms(0));
        assert_eq!(data(&pods, PodSide::Right, later), first);
        let left = data(&pods, PodSide::Left, ms(0));
        assert_ne!(left, first, "a payload each");
        assert_eq!(data(&pods, PodSide::Left, later), left);

        let tail = |pods: &Pods| plaintext(pods, PodSide::Right, later)[12..].to_vec();
        let before = tail(&pods);
        pods.set_battery(Part::Case, Some(49));
        assert_ne!(
            data(&pods, PodSide::Right, later),
            first,
            "a change at once"
        );
        assert_ne!(tail(&pods), before, "the tail with it");

        let before = (plaintext(&pods, PodSide::Right, later), tail(&pods));
        pods.set_lid(false);
        let after = plaintext(&pods, PodSide::Right, later);
        assert_eq!(after[..12], before.0[..12], "the lid is in the clear");
        assert_ne!(tail(&pods), before.1, "the tail all the same");
    }

    #[test]
    fn repairing_changes_the_keys_and_the_address() {
        let mut pods = Pods::new(PRO_3, MAC);
        let keys = pods.keys().clone();
        let address = pods.address_at(PodSide::Right, ms(0));
        pods.repair();
        assert_ne!(pods.keys().enc_key, keys.enc_key);
        assert_ne!(pods.keys().irk, keys.irk);
        assert_ne!(pods.address_at(PodSide::Right, ms(0)), address);
        assert!(!resolves(&keys.irk, pods.address_at(PodSide::Right, ms(0))));
        read(&pods, PodSide::Right, ms(0));

        let fixed = Keys::generate();
        pods.repair_with(fixed.clone());
        assert_eq!(pods.keys(), &fixed);
    }

    /// The opcode of each packet.
    fn opcodes(packets: &[Vec<u8>]) -> Vec<u8> {
        packets.iter().map(|p| p[OPCODE]).collect()
    }

    /// The link coming up as in capture 4 of #71: the handshake's answer with
    /// 0x2B; nothing for the features packet; on the notification request the
    /// battery, 0x08, 0x0C, 0x0E, 0x2E, 0x06 and 0x2E; the key reply when
    /// asked; and once the dump delay is over, the second 0x0C and the dump.
    #[test]
    fn the_link_comes_up_in_the_captured_order() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_battery(Part::Left, Some(83));
        pods.set_charging(Part::Case, true);
        pods.set_in_ear(PodSide::Left, true);
        pods.aap_opened(ms(0));
        assert!(pods.is_connected(), "AAP needs a Bluetooth link");

        pods.receive(&HANDSHAKE, ms(0));
        let answer = sent(&pods.poll(ms(0)));
        assert_eq!(answer[0], HANDSHAKE_ACK);
        assert_eq!(answer[0].len(), 18);
        assert_eq!(
            answer[1],
            known_hosts_packet(&[MAC, THIS_HOST, ANOTHER_HOST])
        );
        assert_eq!(answer.len(), 2);

        let at = ms(10);
        pods.receive(&NOTIFICATIONS, at);
        pods.receive(&FEATURES, at);
        pods.receive(&KEY_REQUEST, at);
        let burst = sent(&pods.poll(at));
        assert_eq!(
            opcodes(&burst),
            [0x04, 0x08, 0x0C, 0x0E, 0x2E, 0x06, 0x2E, 0x31],
            "{burst:02x?}"
        );
        let info = parse_battery_packet(&burst[0]).unwrap();
        assert_eq!(info.left.unwrap().level, Some(83));
        assert_eq!(info.right.unwrap().level, Some(70));
        assert!(info.case.unwrap().is_charging());
        assert_eq!(burst[1], primary_packet(PodSide::Left, true, false));
        assert_eq!(burst[2], in_use_packet(MAC, false));
        assert_eq!(burst[2].len(), 14);
        assert_eq!(
            burst[2][6..],
            [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x01, 0x02]
        );
        assert_eq!(burst[3], host_packet(THIS_HOST));
        assert_eq!(burst[3].len(), 13);
        assert_eq!(burst[4], host_entry_packet(THIS_HOST));
        assert_eq!(burst[4].len(), 17);
        assert_eq!(burst[4][9..15], THIS_HOST.0);
        assert_eq!(burst[5], ears(Place::InEar, Place::InCase));
        let keys = parse_proximity_keys(&burst[7]).unwrap();
        assert_eq!(find_encryption_key(&keys), Ok(pods.keys().enc_key));
        assert_eq!(keys[0].key_type, KeyType::Irk);
        assert_eq!(keys[0].data, pods.keys().irk);

        let dumped = at + DUMP_DELAY;
        assert_eq!(pods.next_due(), Some(dumped));
        assert!(pods.poll(dumped - ms(1)).is_empty(), "no mode yet");
        let dump = sent(&pods.poll(dumped));
        assert_eq!(dump[0], in_use_packet(THIS_HOST, true));
        assert_eq!(dump[0][12..], [0x01, 0x01]);
        assert_eq!(opcodes(&dump[1..4]), [0x02, 0x1D, 0x09]);
        assert_eq!(
            dump[3],
            noise::set_packet(NoiseMode::NoiseCancellation).to_vec()
        );
        assert_eq!(dump.len(), 1 + STARTUP_DUMP.len());
        assert_eq!(pods.next_due(), None);
        assert!(pods.poll(ms(10_000)).is_empty());
    }

    /// The handshake's answer can take a while, as a Pro 3's took a second,
    /// and what the app sends before it is out is dropped.
    #[test]
    fn nothing_counts_before_the_handshake_is_answered() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.handshake_delay = ms(1000);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.receive(&NOTIFICATIONS, ms(10));
        pods.receive(&KEY_REQUEST, ms(10));
        assert!(pods.poll(ms(999)).is_empty());
        assert_eq!(opcodes(&sent(&pods.poll(ms(1000)))), [0x00, 0x2B]);
        assert!(pods.poll(ms(5000)).is_empty(), "dropped");

        pods.receive(&KEY_REQUEST, ms(5000));
        assert_eq!(opcodes(&sent(&pods.poll(ms(5000)))), [0x31]);
        assert_eq!(pods.received().len(), 4);
    }

    /// The dump carries the mode as it is when it comes, and until then no
    /// change of mode is reported.
    #[test]
    fn the_dump_carries_the_mode_as_it_is_then() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.receive(&NOTIFICATIONS, ms(0));
        pods.poll(ms(0));
        pods.set_noise_mode(NoiseMode::Transparency);
        assert!(pods.poll(ms(10)).is_empty());
        let dump = sent(&pods.poll(DUMP_DELAY));
        let modes: Vec<_> = dump.iter().filter(|p| is_noise_mode_packet(p)).collect();
        assert_eq!(
            modes,
            [&noise::set_packet(NoiseMode::Transparency).to_vec()]
        );
        pods.set_noise_mode(NoiseMode::Adaptive);
        assert_eq!(
            sent(&pods.poll(DUMP_DELAY + ms(10))),
            [noise::set_packet(NoiseMode::Adaptive).to_vec()]
        );
    }

    /// The mode is one 0x09 setting among some twenty, several of them with
    /// values that read as a mode. Only byte 6 picks it out. As in the
    /// captures, allow-Off (0x34) is not among them.
    #[test]
    fn the_startup_dump_hides_the_mode_among_other_settings() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_noise_mode(NoiseMode::Adaptive);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.receive(&NOTIFICATIONS, ms(0));
        pods.poll(ms(0));
        let settings: Vec<Vec<u8>> = sent(&pods.poll(DUMP_DELAY))
            .into_iter()
            .filter(|p| p[OPCODE] == 0x09)
            .collect();
        let sub_cmds: Vec<u8> = settings.iter().map(|p| p[6]).collect();
        assert_eq!(
            sub_cmds,
            [
                0x0D, 0x18, 0x17, 0x25, 0x23, 0x1F, 0x24, 0x28, 0x26, 0x29, 0x2C, 0x2F, 0x33, 0x2E,
                0x30, 0x1B, 0x35, 0x3E, 0x37, 0x38, 0x3B
            ]
        );
        assert!(settings.contains(&noise::settings_report(0x24, &[0x00, 0x03]).to_vec()));
        assert!(settings.contains(&noise::settings_report(0x26, &[0x02]).to_vec()));
        assert!(settings.contains(&noise::settings_report(0x28, &[0x02]).to_vec()));

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
        assert!(
            settings
                .iter()
                .all(|p| noise::parse_allow_off_packet(p).is_none())
        );
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

    /// AirPods 4's answers, through the quirks and with Conversation
    /// Awareness on, as theirs was: level 09 on every change; the echo on
    /// every fourth, after the delay, carrying the mode.
    #[test]
    fn answers_mode_changes_as_airpods_4_do() {
        let mut pods = subscribed();
        pods.set_conversation_awareness(true);
        pods.quirks.idle_level_only = true;
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
                [conversation::level_packet(0x09).to_vec()],
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

    /// No echo, and no level with Conversation Awareness off: nothing at all.
    #[test]
    fn the_echo_and_the_levels_can_be_switched() {
        let mut pods = subscribed();
        pods.set_conversation_awareness(true);
        pods.quirks.idle_level_only = true;
        pods.quirks.echo_delay = Duration::ZERO;
        pods.receive(&noise::set_packet(NoiseMode::Adaptive), ms(10));
        assert_eq!(
            sent(&pods.poll(ms(10))),
            [
                conversation::level_packet(0x09).to_vec(),
                noise::set_packet(NoiseMode::Adaptive).to_vec()
            ]
        );

        pods.quirks.echo_every = 0;
        pods.set_conversation_awareness(false);
        for mode in NoiseMode::ALL {
            pods.receive(&noise::set_packet(mode), ms(20));
        }
        assert!(pods.poll(ms(10_000)).is_empty());
    }

    /// A level packet.
    fn level(level: u8) -> Vec<u8> {
        conversation::level_packet(level).to_vec()
    }

    /// A Pro 3 with Conversation Awareness on, as on 2026-11-01 (#150):
    /// every mode change answered level 08 at once, the echo, then 09 - the
    /// refused bare Off too, around a report of the mode it stayed in.
    #[test]
    fn with_conversation_awareness_a_pro_3_frames_a_change_in_levels() {
        let mut pods = subscribed();
        pods.set_conversation_awareness(true);
        let echo = ms(10) + ECHO_DELAY;
        pods.receive(&noise::set_packet(NoiseMode::Transparency), ms(10));
        assert_eq!(sent(&pods.poll(ms(10))), [level(0x08)]);
        assert_eq!(
            sent(&pods.poll(echo)),
            [
                noise::set_packet(NoiseMode::Transparency).to_vec(),
                level(0x09)
            ]
        );

        let later = ms(5000);
        pods.receive(&noise::set_packet(NoiseMode::Off), later);
        assert_eq!(pods.noise_mode(), NoiseMode::Transparency, "refused");
        assert_eq!(sent(&pods.poll(later)), [level(0x08)]);
        assert_eq!(
            sent(&pods.poll(later + ECHO_DELAY)),
            [
                noise::set_packet(NoiseMode::Transparency).to_vec(),
                level(0x09)
            ]
        );
    }

    /// The setting is in the startup dump, as `0x28 01` or `02`, and with it
    /// on level 09 follows the dump. Turned here, it is taken and answered
    /// with nothing; the next connection's dump carries it.
    #[test]
    fn the_dump_carries_conversation_awareness_and_the_command_sets_it() {
        let setting = |dump: &[Vec<u8>]| {
            dump.iter()
                .filter_map(|p| conversation::parse_setting_packet(p).ok())
                .collect::<Vec<_>>()
        };
        let mut pods = Pods::new(PRO_3, MAC);
        let dump = dump_of(&mut pods, ms(0));
        assert_eq!(setting(&dump), [false]);
        assert!(!dump.contains(&level(0x09)));

        pods.receive(&conversation::set_packet(true), ms(10));
        assert!(pods.conversation_awareness());
        assert!(pods.poll(ms(10_000)).is_empty(), "nothing answers it");

        pods.aap_closed();
        let dump = dump_of(&mut pods, ms(20_000));
        assert_eq!(setting(&dump), [true]);
        assert_eq!(dump.last(), Some(&level(0x09)));
    }

    /// The name in a startup dump, as the app reads it.
    fn dumped_name(dump: &[Vec<u8>]) -> Vec<Vec<u8>> {
        dump.iter()
            .filter_map(|p| info::info_name_bytes(p).map(<[u8]>::to_vec))
            .collect()
    }

    /// The name is in the startup dump's 0x1D, laid out as a Pro 3's, with
    /// the `04 00` before it. A rename is taken and answered with nothing;
    /// the next connection's dump carries the new name.
    #[test]
    fn the_dump_carries_the_name_and_a_rename_sets_it() {
        let mut pods = Pods::new(PRO_3, MAC);
        let dump = dump_of(&mut pods, ms(0));
        assert_eq!(
            dump.iter()
                .filter_map(|p| info::parse_device_info(p).ok()?.name.ok())
                .collect::<Vec<_>>(),
            [DEFAULT_NAME]
        );

        pods.receive(&name::rename_packet("Test Pods").unwrap(), ms(10));
        assert_eq!(pods.name(), b"Test Pods");
        assert!(pods.poll(ms(10_000)).is_empty(), "nothing answers it");

        pods.aap_closed();
        let dump = dump_of(&mut pods, ms(20_000));
        assert_eq!(dumped_name(&dump), [b"Test Pods".to_vec()]);
    }

    /// The firmware version in a startup dump, as the app reads it.
    fn dumped_version(dump: &[Vec<u8>]) -> Vec<String> {
        dump.iter()
            .filter_map(|p| info::parse_device_info(p).ok()?.version.ok())
            .map(|version| version.to_string())
            .collect()
    }

    /// The Marketing Version is in field 10 of the startup dump's 0x1D,
    /// as a Pro 3's was, and the next connection's after an update carries
    /// the new one.
    #[test]
    fn the_dump_carries_the_firmware_version() {
        let mut pods = Pods::new(PRO_3, MAC);
        assert_eq!(dumped_version(&dump_of(&mut pods, ms(0))), ["9A348"]);

        pods.set_marketing_version(0x00A0_0010);
        assert!(pods.poll(ms(10_000)).is_empty(), "nothing reports it");
        pods.aap_closed();
        let dump = dump_of(&mut pods, ms(20_000));
        assert_eq!(dumped_version(&dump), ["10A1"]);
    }

    /// Past 127 bytes the pods keep the first 127, as a Pro 3 did in the
    /// name run of #160, a character cut in two included. A name named from
    /// elsewhere is kept whole.
    #[test]
    fn a_long_name_is_cut_at_127_bytes() {
        let mut pods = subscribed();
        let long = "ü".repeat(100);
        pods.receive(&name::rename_packet_unchecked(long.as_bytes()), ms(10));
        assert_eq!(pods.name(), &long.as_bytes()[..NAME_KEPT]);

        pods.set_name(&long);
        assert_eq!(pods.name(), long.as_bytes());
    }

    /// A pair subscribed as [`subscribed`] is, `setup` run on it first, the
    /// replies and the dump taken.
    fn subscribed_with(setup: impl FnOnce(&mut Pods)) -> Pods {
        let mut pods = Pods::new(PRO_3, MAC);
        setup(&mut pods);
        pods.quirks.dump_delay = Duration::ZERO;
        pods.aap_opened(ms(0));
        for packet in [&HANDSHAKE[..], &NOTIFICATIONS, &FEATURES, &KEY_REQUEST] {
            pods.receive(packet, ms(0));
        }
        pods.poll(ms(0));
        pods
    }

    /// The startup dump of `pods`, connected and subscribed at `at`.
    fn dump_of(pods: &mut Pods, at: Duration) -> Vec<Vec<u8>> {
        pods.quirks.dump_delay = Duration::ZERO;
        pods.aap_opened(at);
        pods.receive(&HANDSHAKE, at);
        pods.receive(&NOTIFICATIONS, at);
        sent(&pods.poll(at))
            .into_iter()
            .skip_while(|p| *p != in_use_packet(THIS_HOST, true))
            .collect()
    }

    /// Every packet `pods` sends from `from` to `to`, polled when each falls
    /// due, with the time it went.
    fn timeline(pods: &mut Pods, from: Duration, to: Duration) -> Vec<(Duration, Vec<u8>)> {
        let mut packets: Vec<_> = sent(&pods.poll(from))
            .into_iter()
            .map(|p| (from, p))
            .collect();
        while let Some(at) = pods.next_due().filter(|at| *at <= to) {
            packets.extend(sent(&pods.poll(at)).into_iter().map(|p| (at, p)));
        }
        packets
    }

    /// The first stretch of the #150 run, 6.36 s of speech: level 01, the
    /// mode in use reported, the switch to Transparency, 02; and at the end
    /// 03, 07, 08, the switch back and 09 - each at the run's offset.
    #[test]
    fn a_stretch_of_speech_goes_as_on_the_hardware() {
        let mut pods = subscribed_with(|pods| pods.set_conversation_awareness(true));
        let start = ms(22_030);
        let end = start + ms(6360);
        pods.speak(start, end - start).unwrap();
        assert!(pods.is_speaking(start));
        let nc = noise::set_packet(NoiseMode::NoiseCancellation).to_vec();
        let transparency = noise::set_packet(NoiseMode::Transparency).to_vec();
        assert_eq!(
            timeline(&mut pods, start, end + ms(10_000)),
            [
                (start, level(0x01)),
                (start + SPEECH_MODE_REPORT, nc.clone()),
                (start + SPEECH_TRANSPARENCY, transparency),
                (start + SPEECH_STILL, level(0x02)),
                (end, level(0x03)),
                (end + ms(70), level(0x07)),
                (end + ms(100), level(0x08)),
                (end + SPEECH_MODE_BACK, nc),
                (end + SPEECH_IDLE, level(0x09)),
            ]
        );
        assert_eq!(pods.noise_mode(), NoiseMode::NoiseCancellation);
        assert!(!pods.is_speaking(end + SPEECH_IDLE + ms(1)));
    }

    /// A stretch too short for the switch sends the levels alone, and one
    /// too short for 02 leaves it out.
    #[test]
    fn a_short_stretch_leaves_out_what_it_has_no_room_for() {
        let mut pods = subscribed_with(|pods| pods.set_conversation_awareness(true));
        pods.speak(ms(100), ms(300)).unwrap();
        let (modes, levels): (Vec<_>, Vec<_>) = timeline(&mut pods, ms(100), ms(10_000))
            .into_iter()
            .map(|(_, p)| p)
            .partition(|p| is_noise_mode_packet(p));
        assert_eq!(levels, [0x01, 0x03, 0x07, 0x08, 0x09].map(level), "no 02");
        assert_eq!(
            modes,
            [noise::set_packet(NoiseMode::NoiseCancellation).to_vec()],
            "the mode in use, and no switch"
        );
        assert_eq!(pods.noise_mode(), NoiseMode::NoiseCancellation);
    }

    /// With the setting off, speech brings nothing; with no connection, or
    /// a stretch under way, it is refused.
    #[test]
    fn speech_needs_the_setting_and_a_connection() {
        let mut pods = subscribed();
        pods.speak(ms(10), ms(3000)).unwrap();
        assert_eq!(pods.next_due(), None);
        assert!(!pods.is_speaking(ms(10)));

        pods.set_conversation_awareness(true);
        pods.speak(ms(10), ms(3000)).unwrap();
        assert!(pods.speak(ms(1000), ms(3000)).is_err(), "under way");
        let mut idle = Pods::new(PRO_3, MAC);
        idle.set_conversation_awareness(true);
        assert!(idle.speak(ms(10), ms(3000)).is_err(), "no connection");
    }

    /// A mode set during the stretch takes effect at once, and is the one
    /// the pods go back to: their switch back changes nothing then, and
    /// reports nothing.
    #[test]
    fn a_mode_set_during_speech_is_the_one_gone_back_to() {
        let mut pods = subscribed_with(|pods| pods.set_conversation_awareness(true));
        pods.quirks.echo_every = 0;
        pods.speak(ms(0), ms(3000)).unwrap();
        timeline(&mut pods, ms(0), ms(1000));
        assert_eq!(pods.noise_mode(), NoiseMode::Transparency);

        pods.receive(&noise::set_packet(NoiseMode::Adaptive), ms(1000));
        assert_eq!(pods.noise_mode(), NoiseMode::Adaptive);
        let rest = timeline(&mut pods, ms(1000), ms(10_000));
        assert!(
            rest.iter().all(|(_, p)| !is_noise_mode_packet(p)),
            "{rest:02x?}"
        );
        assert_eq!(pods.noise_mode(), NoiseMode::Adaptive);
    }

    /// A connection that closes in the middle of a stretch takes the switch
    /// back with it, and the pods are back in their mode at once.
    #[test]
    fn a_stretch_cut_short_by_a_hang_up_leaves_the_mode_as_it_was() {
        let mut pods = subscribed_with(|pods| pods.set_conversation_awareness(true));
        pods.speak(ms(0), ms(3000)).unwrap();
        timeline(&mut pods, ms(0), ms(1000));
        assert_eq!(pods.noise_mode(), NoiseMode::Transparency);
        pods.aap_closed();
        assert_eq!(pods.noise_mode(), NoiseMode::NoiseCancellation);
        assert!(!pods.is_speaking(ms(1000)));
    }

    /// The ear report, `primary` then `secondary`.
    fn ears(primary: Place, secondary: Place) -> Vec<u8> {
        ear_packet(EarReport { primary, secondary }).to_vec()
    }

    /// Out of the box but for the left pod worn: the right one in the open
    /// case, the left one primary. Told with the battery, not before.
    #[test]
    fn tells_where_the_pods_are_with_the_battery() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.aap_opened(ms(0));
        pods.set_in_ear(PodSide::Left, true);
        assert!(pods.poll(ms(0)).is_empty(), "nothing before the handshake");

        pods.receive(&HANDSHAKE, ms(0));
        pods.poll(ms(0));
        pods.set_in_ear(PodSide::Right, true);
        assert!(pods.poll(ms(10)).is_empty(), "nothing before the request");
        pods.set_in_ear(PodSide::Right, false);

        pods.receive(&NOTIFICATIONS, ms(20));
        let places: Vec<Vec<u8>> = sent(&pods.poll(ms(20)))
            .into_iter()
            .filter(|p| [0x06, 0x08].contains(&p[OPCODE]))
            .collect();
        assert_eq!(
            places,
            [
                primary_packet(PodSide::Left, true, false).to_vec(),
                ears(Place::InEar, Place::Out),
            ]
        );
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

    /// The switch as the #142 and #71 captures have it: the ear report in the
    /// new order first, then the primary pod twice, the first marked as a
    /// switch. The hardware sends the battery between the two, the fake with
    /// the ear report.
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
                ears(Place::InEar, Place::Out),
                battery_packet(&pods.aap_batteries()),
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

    /// The secondary pod in the case as the lid closes and opens, in the order
    /// of capture 4 of #71: the primary pod packet's say on whether it can be
    /// reached, the place, and the battery, which reports that pod and the
    /// case Disconnected while the lid is closed.
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
                primary_packet(PodSide::Left, false, false).to_vec(),
                ears(Place::InEar, Place::InClosedCase),
                battery_packet(&away),
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
                primary_packet(PodSide::Left, true, false).to_vec(),
                ears(Place::InEar, Place::InCase),
                battery_packet(&back),
            ]
        );
    }

    /// Both pods in the closed case: the pods drop the link, AAP with it, and
    /// nothing is sent first. The lid closing is one way there, a pod going
    /// into the closed case another, and a pair connected without AAP drops
    /// its link the same.
    #[test]
    fn both_pods_in_the_closed_case_drop_the_link() {
        let mut pods = subscribed();
        pods.set_lid(false);
        assert_eq!(pods.poll(ms(10)), [Action::Disconnect]);
        assert!(!pods.is_connected() && !pods.has_aap());
        assert!(pods.poll(ms(20)).is_empty());

        let mut pods = subscribed();
        pods.set_in_ear(PodSide::Left, true);
        pods.set_lid(false);
        assert_eq!(pods.poll(ms(10)).len(), 3, "one pod in the closed case");
        pods.set_in_case(PodSide::Left, true);
        assert_eq!(pods.poll(ms(20)), [Action::Disconnect]);

        let mut pods = Pods::new(PRO_3, MAC);
        pods.set_connected(true);
        pods.set_lid(false);
        assert_eq!(pods.poll(ms(0)), [Action::Disconnect]);
        assert!(!pods.is_connected());
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
        assert_eq!(opcodes(&sent(&pods.poll(ms(0)))), [0x00, 0x2B]);
        assert_eq!(pods.next_due(), Some(ms(500)));
        assert!(pods.poll(ms(499)).is_empty());
        let report = sent(&pods.poll(ms(500)));
        assert!(parse_battery_packet(&report[0]).is_ok(), "{report:02x?}");
        assert_eq!(report.len(), 7, "the battery and what comes with it");
        assert_eq!(pods.next_due(), Some(ms(500) + DUMP_DELAY));
    }

    /// A change while the report waits is not sent ahead of it, to be
    /// overtaken by the state as it was when asked: the report comes as
    /// asked, and the change right after it, so the app's last word is the
    /// state of the moment.
    #[test]
    fn a_change_during_the_report_delay_comes_after_the_report() {
        let mut pods = Pods::new(PRO_3, MAC);
        pods.quirks.report_delay = ms(500);
        pods.aap_opened(ms(0));
        pods.receive(&HANDSHAKE, ms(0));
        pods.receive(&NOTIFICATIONS, ms(0));
        pods.poll(ms(0));
        pods.set_battery(Part::Left, Some(42));
        pods.set_in_ear(PodSide::Left, true);
        assert!(pods.poll(ms(100)).is_empty(), "ahead of the report");

        let packets = sent(&pods.poll(ms(500)));
        let batteries: Vec<_> = packets
            .iter()
            .filter_map(|p| parse_battery_packet(p).ok())
            .map(|info| info.left.unwrap().level)
            .collect();
        assert_eq!(batteries, [Some(80), Some(42)], "as asked, then now");
        let places: Vec<_> = packets.iter().filter(|p| p[OPCODE] == 0x06).collect();
        assert_eq!(
            places,
            [
                &ears(Place::InCase, Place::InCase),
                &ears(Place::InEar, Place::InCase)
            ]
        );
        assert!(pods.poll(ms(600)).is_empty());
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
        let pd = parse_proximity_data(&pods.advertisement(PodSide::Right, ms(20)).data).unwrap();
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
