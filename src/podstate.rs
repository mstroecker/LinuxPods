//! Centralized AirPods state coordination.
//!
//! Coordinates two data sources and notifies consumers:
//!   - AAP (exact) for every device an L2CAP connection is up to - several at once
//!   - BLE advertisements for every other device, once a stored IRK has resolved
//!     the address one comes from and the same pair's ENC_KEY has decrypted it -
//!     which identifies the device and gives its exact levels
//!
//! The choice is per device, not global: an AAP connection to one pair of AirPods
//! must not stop a second pair from being tracked over BLE.
//!
//! # BLE is cached, AAP is not
//!
//! The last advertisement of every device is kept, including the connected
//! device's own advertisements while AAP supersedes them. When the link drops the
//! cached reading takes over at once, rather than the device going blank until
//! it next advertises - which, lid closed in the case, can be a long wait. AAP
//! state is only ever the live link's and goes with it - all but the firmware
//! version, which is the pair's and stays for the run, see
//! [`Inner::firmware_versions`]. While the link is up, the device's snapshot
//! takes from its cached advertisement what AAP does not carry: lid and
//! connection state - and in-ear until the link has reported it, see
//! [`EarState`].
//!
//! The cache outlives the app, too: each cached reading's levels, charging and
//! model are saved in [`readings`], every so often and as the app quits, and
//! loaded at the next start, so a pair put away in its case before a relaunch
//! shows its last reading rather than nothing (#170). Ages are on the wall
//! clock for that, and for a machine that slept - see [`age`].
//!
//! A link supersedes the cached reading from its first battery packet, not
//! from when it comes up: in between - about a second on one pair - the
//! device would show no reading at all, and its GNOME Settings battery would
//! go and come back with every reconnect (#62).
//!
//! # What is kept
//!
//! Only advertisements identified as a pair's - see
//! [`Coordinator::identify_and_decrypt`]. That identifies the device, so its
//! state is keyed by the real MAC and the map stays bounded by the stored keys;
//! [`Inner::prune`] drops readings after [`BLE_CACHE_TTL`]. The rest are dropped on
//! arrival: keyed by a randomized MAC - one per pod, a new one every few
//! minutes and after the lid has been closed - they could never be attributed
//! to anything, and keeping them - an entry and a broadcast each - is exactly
//! what a BLE spam flood feeds on. Nor is an address remembered that resolves
//! with no stored IRK: only those that resolve, a bounded few
//! ([`ResolvedAddresses`]).

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
#[cfg(feature = "harness")]
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};
use tracing::Instrument;

use crate::KEY_LEN;
use crate::aap::ear::{EarReport, EarState, Place, ROLE_SWITCH_HOLD};
use crate::aap::info::{DeviceInfo, FirmwareVersion};
use crate::aap::noise::{ModeState, Report};
use crate::aap::{self, Message};
use crate::ble::decode_model_name;
use crate::ble::decrypt::decrypt_for_device;
use crate::ble::parser::{PodSide, ProximityData};
use crate::ble::rpa::{self, ResolvedAddresses};
use crate::bluez::DeviceNames;
use crate::keystore::{Keystore, PairKeys};
use crate::readings::{self, Reading};

/// How long the last advertisement of an identified device stays on show: a day,
/// as a pair in a closed case says nothing, and its last levels are still a hint
/// of how charged it was (#169). Keyed by the real MAC, these do not rotate, so
/// the map stays bounded by the number of stored keys. Counted on the wall
/// clock - see [`age`].
pub const BLE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long before `now` a reading `heard` then was heard, on the wall clock.
///
/// The wall clock, not [`Instant`]: on Linux that is `CLOCK_MONOTONIC`, which
/// stands still while the machine sleeps, so a reading from before a night's
/// suspend would read "30 min ago" the next morning and expire hours late -
/// and a reading kept across a relaunch ([`crate::readings`]) needs a time
/// that means something to the next run (#170).
///
/// `None` for a time after `now`: the clock was set back since, and how old
/// the reading is cannot be told. Such a reading is dropped at the next prune
/// and on load, and warns about nothing (`notify::fresh`); until it goes, the
/// window shows it as just heard.
pub fn age(heard: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(heard).ok()
}

/// Set and not empty, it names a directory of Unix sockets for
/// [`Coordinator::new`] to open AAP links to in place of L2CAP, one per pair at
/// [`aap::client::unix_socket_path`]: a fake answers there (#74).
#[cfg(feature = "harness")]
pub const AAP_SOCKET_DIR_ENV: &str = "LINUXPODS_AAP_SOCKET_DIR";

/// Which protocol a reading came over. Its `Display` is what the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSource {
    Ble,
    Aap,
}

impl std::fmt::Display for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ble => write!(f, "BLE"),
            Self::Aap => write!(f, "AAP"),
        }
    }
}

/// What a pair of AirPods can be told to do over its AAP link - see
/// [`Coordinator::command`]. The key request is not among them: the link
/// sends it as it comes up, on every connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Switches the noise control mode, and records it.
    NoiseControl(aap::NoiseMode),
    /// Turns Conversation Awareness on or off, and records it.
    ConversationAwareness(bool),
    /// Renames the pods - the name they give every host, not BlueZ's alias,
    /// which nothing here touches - and records the name. Refused before it
    /// is queued when [`aap::name::check`] refuses the name.
    Rename(String),
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoiseControl(mode) => write!(f, "switch to {mode}"),
            Self::ConversationAwareness(enabled) => write!(
                f,
                "switch of Conversation Awareness {}",
                if *enabled { "on" } else { "off" }
            ),
            Self::Rename(name) => write!(f, "rename to {name:?}"),
        }
    }
}

/// How a setting's state reads in a log.
fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// A speech level a link brought, or the link going, as
/// [`Coordinator::speech_events`] tells them, for what lowers the media's
/// volume while the user speaks - see `crate::volume`.
///
/// Events rather than the snapshot's [`PodState::speech_level`]: the watch
/// keeps the latest snapshot alone, so a short stretch's `01` could be gone
/// by the time a slow consumer looked, and the first `01` and the first
/// level after it are what lowering and fading back start on. Nor does a
/// snapshot tell one link to a pair from the next. Here every level comes,
/// in the order the pods sent it, a link's levels begin after its
/// [`SpeechEvent::LinkUp`] and end with its [`SpeechEvent::LinkGone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechEvent {
    /// The pair has a link again: a volume left lowered when its transport
    /// went may be put back once one is there.
    LinkUp { mac: Address },
    /// A speech level from the pair's link - see
    /// [`aap::conversation::Speech`].
    Level { mac: Address, level: u8 },
    /// The pair's link went: closed here, or lost. Its last level stands for
    /// nothing any more.
    LinkGone { mac: Address },
}

/// Where a link's pods are, or the link coming up or going, as
/// [`Coordinator::ear_events`] tells them, for what pauses the media when a
/// pod leaves an ear and plays it again when it is back - see `crate::media`.
///
/// From the link's ear state alone, never the advertisement: a connected
/// pair's advertised in-ear lags its ears, and its two pods contradict each
/// other (#142), while the snapshot falls back to it before the link has
/// reported and after it goes, and does not say where its in-ear came from.
/// Events rather than snapshots, too, as for [`SpeechEvent`]: the watch keeps
/// the latest alone, and a pod out and back in quickly could be gone by the
/// time a slow consumer looked. A link's places come after its
/// [`EarEvent::LinkUp`] and before its [`EarEvent::LinkGone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarEvent {
    /// The pair has a link: nothing is known of its pods yet.
    LinkUp { mac: Address },
    /// Where each pod is, left then right, as the link's ear state reads
    /// once it has an ear report and the primary pod - see [`EarState`] -
    /// told as that changes: the first reading, and every change after it.
    Places {
        mac: Address,
        left: Place,
        right: Place,
    },
    /// The pair's link went: closed here, or lost. Its places stand for
    /// nothing any more.
    LinkGone { mac: Address },
}

/// A command in a link's queue, and where its result goes.
struct Queued {
    command: Command,
    done: oneshot::Sender<Result<()>>,
}

/// A link's command queue, as `Coordinator::queues` holds it: with the
/// link's client, so that it goes only with its own link, as `Inner::links`
/// records do.
struct LinkQueue {
    client: Arc<aap::Client>,
    commands: mpsc::UnboundedSender<Queued>,
}

/// A command being carried out, until its caller has been told how it went.
/// Dropped before that - the link's run ended, or was aborted, in the middle
/// of the command - it tells the caller the connection was lost meanwhile.
struct Underway {
    mac: Address,
    command: Command,
    done: Option<oneshot::Sender<Result<()>>>,
}

impl Underway {
    fn finish(&mut self, result: Result<()>) {
        if let Some(done) = self.done.take() {
            // The caller may have stopped waiting; the command is done anyway.
            let _ = done.send(result);
        }
    }
}

impl Drop for Underway {
    fn drop(&mut self) {
        let lost = ConnectionLost {
            mac: self.mac,
            command: self.command.clone(),
        };
        self.finish(Err(lost.into()));
    }
}

/// What a command's caller hears when the link went while the command was
/// being carried out: it may or may not have reached the device.
#[derive(Debug, thiserror::Error)]
#[error("lost the connection to {mac} during the {command}")]
pub struct ConnectionLost {
    pub mac: Address,
    pub command: Command,
}

/// Which device a reading is from, as its advertisements say. AAP never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Apple's product ID, which names the model - see [`decode_model_name`].
    pub product_id: u16,
    /// The color code, as advertised.
    pub color: u8,
}

impl Identity {
    /// The model's name, or for a product ID we do not know, the ID itself.
    pub fn model_name(&self) -> String {
        decode_model_name(self.product_id).map_or_else(
            || format!("Unknown (0x{:04X})", self.product_id),
            str::to_string,
        )
    }
}

/// Unified state, independent of which source produced it.
///
/// Deliberately without `Default`: each source sets every field it knows and
/// says `None` for the rest, rather than leaving a default to pass for a
/// reading.
#[derive(Debug, Clone)]
pub struct PodState {
    pub source: DataSource,

    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,

    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,

    /// Whether each pod is in an ear. For a device on AAP, from its link's
    /// ear reports once it has both one and the primary pod - see
    /// [`EarState`] - and until then from an advertisement like the
    /// [connection state](PodState::connection_state).
    pub left_in_ear: Option<bool>,
    pub right_in_ear: Option<bool>,

    /// From an advertisement like the
    /// [connection state](PodState::connection_state), and `None` too when the
    /// last one said no pod is in the case. One from a worn pod whose partner is
    /// in the case can't see the lid, and keeps the one last reported.
    pub lid_open: Option<bool>,

    /// Decoded with `ble::decode_connection_state`.
    ///
    /// Only advertisements carry it, along with the lid.
    /// Unlike the identity these change as the earbuds are used, so for a
    /// device on AAP they are taken from its cached advertisement each time a
    /// snapshot is built, and they expire with that cache entry. `None` while
    /// no advertisement is cached. A connected pair's payload changes seldom
    /// and lags what it reports - 4 reached the app in 70 s in #142.
    pub connection_state: Option<u8>,

    /// Active noise control mode. `None` until the connected device reports one:
    /// BLE advertisements do not carry it, and a default would show the wrong
    /// mode as selected in the interface.
    pub noise_mode: Option<aap::NoiseMode>,

    /// Whether Conversation Awareness is on. `None` until the connected
    /// device reports it in its startup dump, and without a link: only AAP
    /// carries it. A change made here is recorded at once; one made on
    /// another device may never be reported, as far as is known.
    pub conversation_awareness: Option<bool>,

    /// The last speech level the link brought - see
    /// [`aap::conversation::Speech`] - while Conversation Awareness is on:
    /// what the media's volume should be as the user speaks or has just
    /// stopped. `None` until one comes, and without a link. A level is a
    /// target, so the snapshot may skip one in between; what acts on the
    /// first `01` of a stretch takes [`Coordinator::speech_events`], which
    /// skips none.
    pub speech_level: Option<u8>,

    /// The pods' own name, the one they report to a host: from the device
    /// information packet at the start of the link, and recorded when a
    /// rename is sent, as nothing answers one (#160). `None` until the link
    /// reports it, and without a link: BLE does not carry it. Not BlueZ's
    /// alias - see [`Snapshot::device_names`].
    pub name: Option<String>,

    /// `None` until an advertisement has identified the device. For one on
    /// AAP it is the link's, which outlives the cached advertisement.
    pub identity: Option<Identity>,

    /// The randomized address the advertisement behind a BLE reading came
    /// from. `None` for AAP, which knows only the real one.
    pub current_ble_mac: Option<Address>,

    /// Whether this is a reading an earlier run saved ([`crate::readings`]),
    /// the pair not heard again since. Shown, with its age, but not exported
    /// to GNOME Settings: a pair connected at login would get yesterday's
    /// level there until its first packet. The next advertisement replaces it
    /// with one that is not, and none turns back, so the battery there is
    /// never withdrawn and exported again for it (#62). False for AAP.
    pub restored: bool,

    /// When the advertisement behind a BLE reading arrived, on the wall clock -
    /// see [`age`]. It may be a cached one up to [`BLE_CACHE_TTL`] old, from
    /// before the app started too ([`crate::readings`]). `None` for AAP, which
    /// is always live.
    pub last_seen: Option<SystemTime>,
}

impl PodState {
    /// A reading from `source` that reports nothing, for tests to fill in.
    #[cfg(test)]
    pub(crate) fn empty(source: DataSource) -> Self {
        Self {
            source,
            left_battery: None,
            right_battery: None,
            case_battery: None,
            left_charging: false,
            right_charging: false,
            case_charging: false,
            left_in_ear: None,
            right_in_ear: None,
            lid_open: None,
            connection_state: None,
            noise_mode: None,
            conversation_awareness: None,
            speech_level: None,
            name: None,
            identity: None,
            current_ble_mac: None,
            restored: false,
            last_seen: None,
        }
    }

    /// Lowest of the two earbuds, which is what the tray and GNOME Settings show.
    pub fn lowest_earbud(&self) -> Option<u8> {
        match (self.left_battery, self.right_battery) {
            (Some(l), Some(r)) => Some(l.min(r)),
            (Some(l), None) => Some(l),
            (None, Some(r)) => Some(r),
            (None, None) => None,
        }
    }
}

/// What consumers receive on every update.
///
/// Which devices exist, and which one to show when nothing else decides, are
/// answered here - by [`Snapshot::devices`] and [`Snapshot::default_device`] -
/// rather than by each consumer: answered three ways, the window said "Connect
/// Your AirPods" while the tray showed the battery of a pair whose key had not
/// arrived, and the two could show different pairs.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The current reading per device, by real MAC. Every key is in
    /// [`Snapshot::devices`], but not every device has a reading: a remembered
    /// pair may be out of range, a new link may not have reported yet.
    pub states: HashMap<Address, PodState>,
    /// Every device with a live AAP link, in the order they connected.
    pub connected_macs: Vec<Address>,
    /// Every MAC we hold an encryption key for, sorted. Independent of whether the
    /// device is currently advertising or connected.
    pub known_keys: Vec<Address>,
    /// BlueZ's names by MAC: each paired device's alias - the name the rest
    /// of the desktop shows for it - and its `Name`. Empty until attachment
    /// reports them, and it stays empty when BlueZ is unreachable, so
    /// consumers need a fallback.
    pub device_names: HashMap<Address, DeviceNames>,
    /// Each pair's firmware version: the last one a link of the pair
    /// reported this run - kept after the link goes, see
    /// [`Inner::firmware_versions`]. A pair with no link this run has none.
    pub firmware_versions: HashMap<Address, FirmwareVersion>,
}

impl Snapshot {
    /// The BlueZ alias for `mac`, when one is known.
    pub fn device_name(&self, mac: Address) -> Option<&str> {
        self.device_names
            .get(&mac)
            .map(|names| names.alias.as_str())
    }

    /// Whether an alias is set in BlueZ for `mac` - by GNOME Settings'
    /// rename, say - so that this computer shows it whatever the pods are
    /// called. False while BlueZ's names are not known.
    pub fn has_alias(&self, mac: Address) -> bool {
        self.device_names
            .get(&mac)
            .is_some_and(DeviceNames::has_alias)
    }

    /// The name the app gives `mac` where it names a pair - the switcher, a
    /// low battery notification: the one the rest of the desktop shows once
    /// BlueZ has caught up. An alias set in BlueZ first, as this computer
    /// shows it whatever the pods are called; else the pods' own name, where
    /// the link has reported it or a rename recorded it - BlueZ's `Name`, and
    /// the alias that follows it, keep the old name until the pair connects
    /// again, so a rename showed in the switcher only then (#160); else
    /// BlueZ's alias, then the model. `None` when nothing names it.
    ///
    /// But for an alias typed back to the pods' old name: [`has_alias`] reads
    /// it as unset until the pair connects again, so the pods' new name shows
    /// until then, and the alias after.
    ///
    /// Not the Info group's Name row, which shows the pods' name before any
    /// alias: that row is what a rename changes.
    ///
    /// [`has_alias`]: Snapshot::has_alias
    pub fn display_name(&self, mac: Address) -> Option<String> {
        let state = self.states.get(&mac);
        let alias = self.device_name(mac).map(str::to_string);
        let pods = state.and_then(|state| state.name.clone());
        if self.has_alias(mac) {
            alias
        } else {
            pods.or(alias)
        }
        .or_else(|| {
            state
                .and_then(|state| state.identity)
                .map(|identity| identity.model_name())
        })
    }

    /// The firmware version `mac` last reported this run, connected or not.
    pub fn firmware_version(&self, mac: Address) -> Option<FirmwareVersion> {
        self.firmware_versions.get(&mac).copied()
    }

    pub fn is_connected(&self, mac: Address) -> bool {
        self.connected_macs.contains(&mac)
    }

    /// Every device there is to show, sorted by MAC: each one we hold a key for,
    /// and each one on AAP - whose key may not have arrived, or may never.
    ///
    /// Neither set takes in strangers: an advertisement not identified as a
    /// pair's is never stored, and an AAP link is only opened to attached AirPods. Nor does
    /// either depend on a reading being present, so the list holds still as
    /// advertisements come and go and cached readings expire, rather than a
    /// device flickering in and out of the switcher.
    pub fn devices(&self) -> Vec<Address> {
        let mut macs: Vec<Address> = self
            .known_keys
            .iter()
            .chain(&self.connected_macs)
            .copied()
            .collect();
        macs.sort_unstable();
        macs.dedup();
        macs
    }

    /// The device to show when nothing else decides - the tray always, the
    /// window until the user picks one: the device that has been on AAP longest,
    /// else the first device with a reading, else the first device.
    ///
    /// The longest link rather than the newest, so a second pair connecting does
    /// not take over. In MAC order rather than whichever the `HashMap` yields
    /// first, since that order changes from one snapshot to the next.
    pub fn default_device(&self) -> Option<Address> {
        if let Some(&mac) = self.connected_macs.first() {
            return Some(mac);
        }
        let devices = self.devices();
        devices
            .iter()
            .find(|mac| self.states.contains_key(*mac))
            .or(devices.first())
            .copied()
    }
}

struct Entry {
    state: PodState,
    /// On the wall clock - see [`age`].
    last_seen: SystemTime,
}

impl Entry {
    /// The entry for a reading an earlier run saved: what an advertisement
    /// would have left, but for what was stale by the time it loaded -
    /// in-ear, the lid, the connection state - and the address it came from.
    fn restored(reading: &Reading) -> Self {
        Self {
            state: PodState {
                source: DataSource::Ble,
                left_battery: reading.left_battery,
                right_battery: reading.right_battery,
                case_battery: reading.case_battery,
                left_charging: reading.left_charging,
                right_charging: reading.right_charging,
                case_charging: reading.case_charging,
                left_in_ear: None,
                right_in_ear: None,
                lid_open: None,
                connection_state: None,
                noise_mode: None,
                conversation_awareness: None,
                speech_level: None,
                name: None,
                identity: Some(reading.identity),
                current_ble_mac: None,
                restored: true,
                // Stamped from the entry in `snapshot`.
                last_seen: None,
            },
            last_seen: reading.heard,
        }
    }
}

/// One device's AAP connection.
struct Link {
    /// Shared with the link's read loop, which reads this client and no other.
    /// Its socket takes &self for both send and recv, so a read parked in the
    /// loop never blocks a command. A loop that looked its client up by MAC
    /// would, once the link was replaced, read the new socket under the old
    /// identity - which stored one device's key under another's MAC.
    client: Arc<aap::Client>,
    /// `None` until the first battery packet.
    reading: Option<PodState>,
    /// Unknown until the device reports one or a command sets it. Kept apart
    /// from the reading, which every battery packet replaces, and merged in by
    /// [`Inner::snapshot`]. It goes with the link: a mode from an earlier
    /// session would show as selected after a reconnect, though it may have
    /// been changed from a phone meanwhile.
    noise_mode: ModeState,
    /// Whether Conversation Awareness is on: `None` until the startup dump
    /// reports it or a command sets it. Kept apart from the reading like the
    /// mode, and goes with the link like it.
    conversation_awareness: Option<bool>,
    /// The last speech level: `None` until one comes. Kept apart from the
    /// reading like the mode, and goes with the link like it.
    speech_level: Option<u8>,
    /// The pods' own name: `None` until the startup dump's device
    /// information reports it or a rename sets it. Kept apart from the
    /// reading like the mode, and goes with the link like it: renamed from
    /// another device meanwhile, the next link reports the new name.
    name: Option<String>,
    /// Which pod is in an ear, as the device's ear reports and primary pod
    /// packets tell it. Kept apart from the reading like the mode, and goes
    /// with the link like it: after a disconnect the cached advertisement
    /// shows through.
    ear: EarState,
    /// Which device the link is to, as last advertised; AAP never says.
    /// Kept apart from the reading like the mode. Taken from the cached
    /// advertisement when the link comes up and refreshed by every one after,
    /// so it outlives that cache entry's expiry.
    identity: Option<Identity>,
    /// Orders `Snapshot::connected_macs`.
    since: Instant,
}

/// The parts of an advertisement that change as the earbuds are used - a pod
/// taken out, the lid opened, a call coming in. A linked device's snapshot
/// takes them from its cached advertisement, all but in-ear once its link has
/// reported that: AAP carries in-ear too, and at once, where a connected
/// pair's payload changes seldom and lags its ears (#142). The lid and the
/// connection state it does not carry, as far as is known.
///
/// Not the primary pod: each pod advertises as itself and calls itself
/// primary (#71), so an advertisement's primary flag says only which pod
/// sent it, and two from the two pods of a pair in one state differ in it.
/// Taken into a snapshot, it broadcast one for a linked pair whenever the
/// other pod was heard (#152). The link's own primary pod maps its ear
/// reports, and is not shown.
#[derive(Clone, Copy, PartialEq)]
struct Live {
    left_in_ear: Option<bool>,
    right_in_ear: Option<bool>,
    lid_open: Option<bool>,
    connection_state: Option<u8>,
}

impl Live {
    fn of(state: &PodState) -> Self {
        Self {
            left_in_ear: state.left_in_ear,
            right_in_ear: state.right_in_ear,
            lid_open: state.lid_open,
            connection_state: state.connection_state,
        }
    }

    fn apply_to(self, state: &mut PodState) {
        state.left_in_ear = self.left_in_ear;
        state.right_in_ear = self.right_in_ear;
        state.lid_open = self.lid_open;
        state.connection_state = self.connection_state;
    }
}

struct Inner {
    /// The last advertisement per device - BLE only - or, until the pair is
    /// heard again, the reading an earlier run saved (`crate::readings`). The
    /// connected device's entry keeps updating underneath AAP, so it is
    /// current when the link drops.
    ble: HashMap<Address, Entry>,
    /// One per live AAP link, keyed by real MAC.
    links: HashMap<Address, Link>,
    /// Each pair's ENC_KEY, and its IRK once one has arrived.
    keys: HashMap<Address, PairKeys>,
    device_names: HashMap<Address, DeviceNames>,
    /// Each pair's firmware version, from the device information of its
    /// links. Unlike the rest of AAP, kept after the link goes, for the
    /// app's run: it changes only with an update, which the next link
    /// reports, and a bug report needs it whether the pair is connected or
    /// not. Never pruned, unlike `ble`: bounded by the pairs linked to,
    /// which are attached AirPods alone.
    firmware_versions: HashMap<Address, FirmwareVersion>,
}

impl Inner {
    /// True when a live AAP link makes this device's BLE advertisement
    /// redundant: once the link has a reading - see the module docs. Per
    /// device: every other device's advertisements still count.
    fn supersedes_ble(&self, advertising_mac: Address) -> bool {
        self.links
            .get(&advertising_mac)
            .is_some_and(|link| link.reading.is_some())
    }

    /// The device's link, while it is still `client`'s. A packet read, or a
    /// command sent, just before the link was replaced belongs to the old
    /// session, and must not land on the new one.
    fn live_link(&mut self, mac_addr: Address, client: &Arc<aap::Client>) -> Option<&mut Link> {
        self.links
            .get_mut(&mac_addr)
            .filter(|l| Arc::ptr_eq(&l.client, client))
    }

    /// Records a mode a command set on the link `client` belongs to,
    /// returning whether that changed anything.
    fn record_noise_mode(
        &mut self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        mode: aap::NoiseMode,
    ) -> bool {
        self.live_link(mac_addr, client)
            .is_some_and(|link| link.noise_mode.set(mode, Instant::now()))
    }

    /// Takes a mode the device reported on the link `client` belongs to -
    /// unless it is stale, see [`ModeState`]. `None` once the link is gone.
    fn report_noise_mode(
        &mut self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        mode: aap::NoiseMode,
    ) -> Option<Report> {
        self.live_link(mac_addr, client)
            .map(|link| link.noise_mode.report(mode, Instant::now()))
    }

    /// Drops BLE readings older than [`BLE_CACHE_TTL`], returning whether any went.
    /// One heard after `now` goes too: see [`age`].
    fn prune(&mut self, now: SystemTime) -> bool {
        let before = self.ble.len();
        self.ble
            .retain(|_, e| age(e.last_seen, now).is_some_and(|age| age < BLE_CACHE_TTL));
        self.ble.len() != before
    }

    /// What [`readings`] keeps of each cached advertisement, for the next run.
    fn readings(&self) -> HashMap<Address, Reading> {
        self.ble
            .iter()
            .filter_map(|(&mac, entry)| {
                let state = &entry.state;
                let reading = Reading {
                    left_battery: state.left_battery,
                    right_battery: state.right_battery,
                    case_battery: state.case_battery,
                    left_charging: state.left_charging,
                    right_charging: state.right_charging,
                    case_charging: state.case_charging,
                    // Every advertisement cached has one.
                    identity: state.identity?,
                    heard: entry.last_seen,
                };
                Some((mac, reading))
            })
            .collect()
    }

    /// What a snapshot shows for a device on AAP: its link's reading, with
    /// the rest merged in. `None` until the link's first battery packet.
    fn linked_state(&self, mac: Address, link: &Link) -> Option<PodState> {
        let mut state = link.reading.clone()?;
        state.noise_mode = link.noise_mode.mode();
        state.conversation_awareness = link.conversation_awareness;
        state.speech_level = link.speech_level;
        state.name.clone_from(&link.name);
        // Merged here rather than copied into each battery packet: a
        // reading that arrived before the first advertisement kept a blank
        // model until the next packet, which over AAP comes only when a
        // level changes.
        state.identity = link.identity;
        // From the cache entry itself rather than the link, unlike the
        // identity: these go stale, so they expire with the advertisement
        // they came from.
        if let Some(entry) = self.ble.get(&mac) {
            Live::of(&entry.state).apply_to(&mut state);
        }
        // The link's, where it has reported them: a connected pair's payload
        // changes seldom, and the icons lagged or missed changes (#142). A
        // pod whose place is unknown keeps the advertisement's.
        if let Some((left, right)) = link.ear.in_ear() {
            state.left_in_ear = left.or(state.left_in_ear);
            state.right_in_ear = right.or(state.right_in_ear);
        }
        Some(state)
    }

    /// The [`Live`] parts a snapshot shows for a device on AAP: `None`
    /// without a link, or before its first battery packet.
    fn linked_live(&self, mac: Address) -> Option<Live> {
        let state = self.linked_state(mac, self.links.get(&mac)?)?;
        Some(Live::of(&state))
    }

    fn snapshot(&self) -> Snapshot {
        let mut known_keys: Vec<Address> = self.keys.keys().copied().collect();
        known_keys.sort_unstable();

        let mut states: HashMap<Address, PodState> = self
            .ble
            .iter()
            // Shown until the link's first packet, while the controls stay
            // insensitive: hidden, the device showed no reading at all
            // meanwhile, and its GNOME Settings battery went with it.
            .filter(|&(&mac, _)| !self.supersedes_ble(mac))
            .map(|(&mac, entry)| {
                let mut state = entry.state.clone();
                state.last_seen = Some(entry.last_seen);
                (mac, state)
            })
            .collect();

        for (&mac, link) in &self.links {
            if let Some(state) = self.linked_state(mac, link) {
                states.insert(mac, state);
            }
        }

        let mut links: Vec<(&Address, &Link)> = self.links.iter().collect();
        links.sort_by_key(|(_, link)| link.since);

        Snapshot {
            states,
            connected_macs: links.into_iter().map(|(&mac, _)| mac).collect(),
            known_keys,
            device_names: self.device_names.clone(),
            firmware_versions: self.firmware_versions.clone(),
        }
    }
}

/// Where `Inner::keys` is saved, and what was last saved there.
struct Persistence {
    /// `None` when keys are not saved this session: the data directory could not
    /// be set up, or a key file that did not load could not be moved out of the
    /// way. Keys still arrive and decrypt BLE either way - from memory.
    keystore: Option<Keystore>,
    /// The keys as last loaded or saved successfully. A save is due whenever
    /// memory differs from this. Judged against memory instead, a key whose save
    /// failed looked unchanged the next time it arrived, and was never written.
    saved: HashMap<Address, PairKeys>,
}

/// Where `Inner::ble`'s readings are saved for the next run, and what was
/// last saved there - see [`Coordinator::save_readings`].
struct SavedReadings {
    /// `None` when they are not saved: the keystore's data directory could
    /// not be set up, or its key file not moved out of the way.
    store: Option<readings::Store>,
    /// The readings as last loaded or saved successfully: a save is judged
    /// against these, so one that failed is tried again.
    saved: HashMap<Address, Reading>,
    /// Whether the last save failed, so a disk that keeps refusing warns once.
    failing: bool,
}

/// The readings an earlier run saved in `store` that are worth putting on
/// show at `now`: of a pair with keys, heard less than [`BLE_CACHE_TTL`]
/// ago, and not after `now` - see [`age`]. A file that does not load is set
/// aside, and none are.
fn load_readings(
    store: &readings::Store,
    keys: &HashMap<Address, PairKeys>,
    now: SystemTime,
) -> HashMap<Address, Reading> {
    let mut loaded = match store.load() {
        Ok(loaded) => loaded,
        Err(e) => {
            // Left in place it would do no harm - the next save replaces it -
            // but it would be gone without a trace.
            match store.set_aside() {
                Ok(aside) => tracing::warn!(
                    "failed to load the last readings: {e:#}; moved the file to {} and \
                     starting without them",
                    aside.display()
                ),
                Err(set_aside) => tracing::warn!(
                    "failed to load the last readings: {e:#}, nor could the file be moved \
                     aside ({set_aside:#}); starting without them, and the next save \
                     replaces it"
                ),
            }
            return HashMap::new();
        }
    };
    let read = loaded.len();
    loaded.retain(|mac, reading| {
        keys.contains_key(mac) && age(reading.heard, now).is_some_and(|age| age < BLE_CACHE_TTL)
    });
    if read > 0 {
        tracing::info!(
            "Loaded the last readings of {} pair(s) from disk; left out {} too old, \
             heard after now, or of a pair with no key",
            loaded.len(),
            read - loaded.len()
        );
    }
    loaded
}

/// A device's AAP link as [`Coordinator::open_aap`] opens it, for
/// [`AapLink::run`] to read, and to carry out the commands sent to it.
///
/// Dropped, it closes the link - the connection at once, the coordinator's
/// record of it soon after - whether it was run or not, and wherever its run
/// stopped: the link lasts no longer than whatever holds it. Its own record
/// only, never a newer link's to the device, as the record goes by client.
/// The commands still in its queue go with it, and each one's caller is told
/// it was not done.
pub struct AapLink {
    coordinator: Arc<Coordinator>,
    mac: Address,
    client: Arc<aap::Client>,
    /// The commands [`Coordinator::command`] queued for the link, in the
    /// order they were sent.
    queue: mpsc::UnboundedReceiver<Queued>,
    /// Set once [`AapLink::run`] has seen the record go, leaving nothing for
    /// the drop to do.
    ended: bool,
}

/// How an AAP link ended, as [`AapLink::run`] tells.
#[derive(Debug)]
pub enum LinkEnd {
    /// Closed here, by [`Coordinator::disconnect_aap`].
    Closed,
    /// Gone on its own, with the read that failed: the device hung up, the
    /// connection went with the Bluetooth link - see
    /// [`aap::client::is_link_gone`] - or something went wrong on it.
    Lost(anyhow::Error),
}

impl AapLink {
    /// Reads the link's packets until it ends, carrying out its commands
    /// meanwhile, then takes it down, and says how it ended. Logs nothing of
    /// it: what is worth saying is the caller's to judge.
    ///
    /// The commands are carried out here, in the run, rather than in a task
    /// of their own: they go with the link wherever the run stops, and none
    /// can outlive it to send on a newer link's behalf.
    pub async fn run(mut self) -> LinkEnd {
        let reading = self.coordinator.aap_read_loop(self.mac, &self.client);
        let commanding = self
            .coordinator
            .command_loop(self.mac, &self.client, &mut self.queue);
        // Under the device's MAC, as every line the session's setup logged.
        let error = async {
            tokio::select! {
                error = reading => error,
                never = commanding => match never {},
            }
        }
        .instrument(aap::client::link_span(self.mac))
        .await;
        // Gone already when it was closed on purpose.
        let lost = self
            .coordinator
            .drop_link(self.mac, Some(&self.client))
            .await;
        self.ended = true;
        if lost {
            LinkEnd::Lost(error)
        } else {
            LinkEnd::Closed
        }
    }
}

impl Drop for AapLink {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        // Wakes a read parked in recv, and closes the connection.
        self.client.shutdown();
        // The record needs the lock, which a drop cannot wait for. Without a
        // runtime - it is shutting down - there is no one left to read it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (coordinator, mac, client) =
                (self.coordinator.clone(), self.mac, self.client.clone());
            runtime.spawn(async move {
                coordinator.drop_link(mac, Some(&client)).await;
            });
        }
    }
}

/// Coordinates sources and broadcasts snapshots.
pub struct Coordinator {
    inner: RwLock<Inner>,
    /// Each live link's command queue, by MAC - see [`Coordinator::command`].
    /// Apart from `inner`, so that a command takes its place in the queue
    /// when it is sent, with nothing to wait for. Changed only together with
    /// `Inner::links`, under its write lock, and an entry removed only by its
    /// own link, matched by client, so a MAC has a queue exactly while it has
    /// a link and it is that link's. Never held across an await.
    queues: std::sync::Mutex<HashMap<Address, LinkQueue>>,
    /// The advertising addresses a stored IRK resolved - see
    /// [`Coordinator::resolve`]. Taken alone and never held across an await.
    resolved: std::sync::Mutex<ResolvedAddresses>,
    /// Where [`SpeechEvent`]s go - see [`Coordinator::speech_events`]. Taken
    /// under `inner`'s write lock, so events go out in the order the links
    /// changed, or alone; never held across an await.
    speech: std::sync::Mutex<Vec<mpsc::UnboundedSender<SpeechEvent>>>,
    /// Where [`EarEvent`]s go - see [`Coordinator::ear_events`]. Taken under
    /// `inner`'s write lock, as `speech` is, or alone; never held across an
    /// await.
    ears: std::sync::Mutex<Vec<mpsc::UnboundedSender<EarEvent>>>,
    /// Held across a save, so saves never overlap - they share a temporary
    /// file. Owned by the save while it runs - see
    /// `Coordinator::store_keys`.
    keystore: Arc<Mutex<Persistence>>,
    /// Held across a save of the readings, so saves never overlap - they
    /// share a temporary file. Owned by the save while it runs, as the
    /// keystore's is - see `Coordinator::save_readings`.
    readings: Arc<Mutex<SavedReadings>>,
    /// The latest snapshot, which every subscriber sees - see [`Coordinator::subscribe`].
    updates: watch::Sender<Snapshot>,
    /// Where [`Coordinator::open_aap`] opens links in place of L2CAP, under
    /// the harness. Fixed at construction, so every link of a session goes the
    /// same way.
    #[cfg(feature = "harness")]
    aap_socket_dir: Option<PathBuf>,
    /// How long a link holds an ear report for a role switch at most - see
    /// [`EarState`]. [`ROLE_SWITCH_HOLD`] but under the harness, where a
    /// test may ask for longer.
    role_switch_hold: Duration,
    /// Runs once, in the next broadcast just before it sends, for a test to
    /// hold an update up there.
    #[cfg(test)]
    before_send: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, in the next keystore save ([`Coordinator::store_keys`])
    /// just before it writes, for a test to hold that save up there.
    #[cfg(test)]
    before_save: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Coordinator {
    /// Loads persisted encryption keys so BLE decryption works from first launch.
    ///
    /// Without a usable data directory it starts anyway and keeps keys in memory
    /// only: a key not saved costs only the pair's BLE readings after a restart,
    /// until its next AAP connection fetches the key again.
    ///
    /// Under the harness, `AAP_SOCKET_DIR_ENV` sends AAP to a fake instead - see
    /// `Coordinator::with_aap_socket_dir`.
    pub async fn new() -> Result<Arc<Self>> {
        let keystore = Keystore::new()
            .inspect_err(|e| tracing::warn!("encryption keys will not be saved: {e:#}"))
            .ok();
        #[cfg(feature = "harness")]
        if let Some(dir) = std::env::var_os(AAP_SOCKET_DIR_ENV).filter(|d| !d.is_empty()) {
            return Ok(Self::with_aap_socket_dir(keystore, dir.into()));
        }
        Ok(Self::with_keystore(keystore))
    }

    /// [`Coordinator::new`] with the keystore given, so tests use their own
    /// directory rather than the user's.
    fn with_keystore(keystore: Option<Keystore>) -> Arc<Self> {
        Arc::new(Self::from_keystore(keystore))
    }

    /// A coordinator whose AAP links go to the Unix sockets in `dir`, one per
    /// pair at [`aap::client::unix_socket_path`], where a fake answers for each
    /// device. Never over L2CAP: a pair with no socket there fails to connect.
    ///
    /// Takes the directory, and the keystore, rather than reading them from the
    /// environment, so tests can build several in one process: setting a
    /// variable is process-wide, and `unsafe` since edition 2024.
    #[cfg(feature = "harness")]
    pub fn with_aap_socket_dir(keystore: Option<Keystore>, dir: PathBuf) -> Arc<Self> {
        Self::with_aap_socket_dir_and_hold(keystore, dir, ROLE_SWITCH_HOLD)
    }

    /// [`Coordinator::with_aap_socket_dir`], its links holding an ear report
    /// for a role switch for `hold` at most - see [`EarState`]. Longer than
    /// [`ROLE_SWITCH_HOLD`], it keeps a test that watches a switch from
    /// timing out on a loaded machine before the fake's primary pod packet
    /// has come.
    #[cfg(feature = "harness")]
    pub fn with_aap_socket_dir_and_hold(
        keystore: Option<Keystore>,
        dir: PathBuf,
        hold: Duration,
    ) -> Arc<Self> {
        tracing::info!(
            "Harness: AAP links go to {}/<MAC>.sock instead of L2CAP",
            dir.display()
        );
        Arc::new(Self {
            aap_socket_dir: Some(dir),
            role_switch_hold: hold,
            ..Self::from_keystore(keystore)
        })
    }

    /// Loads the keystore's keys, and the readings saved beside them, into a
    /// coordinator that opens links over L2CAP.
    fn from_keystore(mut keystore: Option<Keystore>) -> Self {
        let mut loaded = HashMap::new();
        if let Some(ks) = &keystore {
            match ks.load() {
                Ok(keys) => {
                    if !keys.is_empty() {
                        let irks = keys.values().filter(|k| k.irk.is_some()).count();
                        tracing::info!(
                            "Loaded the keys of {} pair(s) from disk, {irks} with an IRK",
                            keys.len()
                        );
                    }
                    loaded = keys;
                }
                // Left in place, the file would be overwritten by the next key to
                // arrive - holding that key alone, every other one lost.
                Err(e) => match ks.set_aside() {
                    Ok(aside) => tracing::warn!(
                        "failed to load encryption keys: {e:#}; moved the file to {} \
                         and starting without them",
                        aside.display()
                    ),
                    Err(set_aside) => {
                        tracing::warn!(
                            "failed to load encryption keys: {e:#}; nor could the file be \
                             moved aside ({set_aside:#}), so keys will not be saved"
                        );
                        keystore = None;
                    }
                },
            }
        }

        // Beside the keys, so a test's or the fake world's keystore keeps them
        // out of the user's data directory too.
        let readings_store = keystore
            .as_ref()
            .map(|ks| readings::Store::with_dir(ks.dir().to_path_buf()));
        let restored = readings_store
            .as_ref()
            .map(|store| load_readings(store, &loaded, SystemTime::now()))
            .unwrap_or_default();
        let saved_readings = SavedReadings {
            store: readings_store,
            saved: restored.clone(),
            failing: false,
        };

        let persistence = Persistence {
            keystore,
            saved: loaded.clone(),
        };
        let inner = Inner {
            ble: restored
                .iter()
                .map(|(&mac, reading)| (mac, Entry::restored(reading)))
                .collect(),
            links: HashMap::new(),
            keys: loaded,
            device_names: HashMap::new(),
            firmware_versions: HashMap::new(),
        };
        let (updates, _) = watch::channel(inner.snapshot());

        Self {
            inner: RwLock::new(inner),
            queues: std::sync::Mutex::default(),
            resolved: std::sync::Mutex::default(),
            speech: std::sync::Mutex::default(),
            ears: std::sync::Mutex::default(),
            keystore: Arc::new(Mutex::new(persistence)),
            readings: Arc::new(Mutex::new(saved_readings)),
            updates,
            #[cfg(feature = "harness")]
            aap_socket_dir: None,
            role_switch_hold: ROLE_SWITCH_HOLD,
            #[cfg(test)]
            before_send: std::sync::Mutex::default(),
            #[cfg(test)]
            before_save: std::sync::Mutex::default(),
        }
    }

    /// Registers a new consumer, which sees the latest snapshot: one value
    /// every receiver shares, latest-wins, marked changed so the current state
    /// arrives at once (`docs/app/state.md#snapshots`).
    ///
    /// Never hold the receiver's `borrow()` across an await: it is a read lock,
    /// and every broadcast waits for it under the coordinator's lock.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        let mut rx = self.updates.subscribe();
        rx.mark_changed();
        rx
    }

    /// Registers a consumer of every link's speech levels, and of each link
    /// coming up and going - see [`SpeechEvent`] - from now on.
    ///
    /// Each event is queued, none skipped: a level is a target, but the
    /// first `01` of a stretch and the first level after it are what a
    /// consumer acts on. They come from the user's own pods over AAP, a
    /// handful per stretch of speech, nothing an advertiser can send. A
    /// consumer that is dropped is forgotten at the next event.
    pub fn speech_events(&self) -> mpsc::UnboundedReceiver<SpeechEvent> {
        let (events, receiver) = mpsc::unbounded_channel();
        self.speech_consumers().push(events);
        receiver
    }

    /// Hands `event` to every consumer of [`Coordinator::speech_events`].
    /// Called under `inner`'s write lock, so the events go out in the order
    /// the links changed: a link's levels before its going.
    fn tell_speech(&self, event: SpeechEvent) {
        self.speech_consumers()
            .retain(|consumer| consumer.send(event).is_ok());
    }

    /// The consumers of speech events - see `Coordinator::speech`.
    fn speech_consumers(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<mpsc::UnboundedSender<SpeechEvent>>> {
        self.speech.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a consumer of where every link's pods are, and of each link
    /// coming up and going - see [`EarEvent`] - from now on.
    ///
    /// Each event is queued, none skipped: a pod out and back in is two
    /// changes, and what pauses the media acts on each. They come from the
    /// user's own pods over AAP, one per change of a pod's place, nothing an
    /// advertiser can send. A consumer that is dropped is forgotten at the
    /// next event.
    pub fn ear_events(&self) -> mpsc::UnboundedReceiver<EarEvent> {
        let (events, receiver) = mpsc::unbounded_channel();
        self.ear_consumers().push(events);
        receiver
    }

    /// Hands `event` to every consumer of [`Coordinator::ear_events`].
    /// Called under `inner`'s write lock, so the events go out in the order
    /// the links changed: a link's places before its going.
    fn tell_ears(&self, event: EarEvent) {
        self.ear_consumers()
            .retain(|consumer| consumer.send(event).is_ok());
    }

    /// The consumers of ear events - see `Coordinator::ears`.
    fn ear_consumers(&self) -> std::sync::MutexGuard<'_, Vec<mpsc::UnboundedSender<EarEvent>>> {
        self.ears.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the snapshot every subscriber sees with one of `inner`.
    ///
    /// Takes the state, not a snapshot, so that it is sent while the lock it
    /// was changed under is still held, and snapshots go out in the order the
    /// state changed. Sent after the lock was released, an update in between
    /// could send a newer snapshot first, and the older one landed last - on
    /// show until the next broadcast (#88).
    ///
    /// Sending never awaits, so the lock is not held for long: it waits only
    /// for an open `borrow()`, which no consumer holds across an await.
    fn broadcast(&self, inner: &Inner) {
        #[cfg(test)]
        {
            // Taken out first: a hook that waits must not hold up other broadcasts.
            let hook = self.before_send.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        self.updates.send_replace(inner.snapshot());
    }

    /// Replaces the BlueZ alias map and broadcasts if anything changed.
    ///
    /// Names are display-only, so a snapshot is only worth sending when they
    /// actually differ - attachment re-reads them on every connection event
    /// BlueZ confirms and every adapter change while BlueZ is on the bus, and
    /// rebroadcasting an identical map would wake the UI,
    /// tray, attachment and notifier for nothing.
    pub(crate) async fn set_device_names(&self, names: HashMap<Address, DeviceNames>) {
        let mut inner = self.inner.write().await;
        if inner.device_names == names {
            return;
        }
        inner.device_names = names;
        self.broadcast(&inner);
    }

    /// Number of devices a snapshot would show.
    pub async fn device_count(&self) -> usize {
        self.inner.read().await.snapshot().states.len()
    }

    /// Drops expired BLE readings, broadcasting if any went.
    ///
    /// Pruning also happens on every advertisement, but a cached reading has to
    /// expire even when nothing is advertising at all - that is exactly when it is
    /// on show.
    pub async fn expire(&self) {
        let mut inner = self.inner.write().await;
        if inner.prune(SystemTime::now()) {
            self.broadcast(&inner);
        }
    }

    /// Saves each cached reading for the next run (#170), if
    /// [`readings::due`] says they are worth a write against those last
    /// saved: `refresh` is [`readings::HEARD_REFRESH`] on the expiry clock,
    /// so a pair in range costs a write every so often rather than each
    /// minute, and `Duration::ZERO` as the app quits. Never called per
    /// advertisement: see `docs/app/readings.md#saving`.
    ///
    /// `inner` is held only to copy the readings out, never across the
    /// write. The write runs on the blocking pool and owns the lock while
    /// it does, as the keystore's save: dropped part way - its task aborted -
    /// it runs on, and no other save starts meanwhile. A save
    /// that fails is tried again at the next call.
    pub async fn save_readings(&self, refresh: Duration) {
        let mut saving = self.readings.clone().lock_owned().await;
        let Some(store) = saving.store.clone() else {
            return;
        };
        let current = self.inner.read().await.readings();
        if !readings::due(&saving.saved, &current, refresh) {
            return;
        }
        let written = tokio::task::spawn_blocking(move || match store.save(&current) {
            Ok(()) => {
                if saving.failing {
                    tracing::info!("saved the last readings again");
                }
                tracing::debug!(
                    "saved the last readings of {} pair(s) to {}",
                    current.len(),
                    store.path().display()
                );
                saving.saved = current;
                saving.failing = false;
            }
            Err(e) => {
                if !saving.failing {
                    tracing::warn!(
                        "failed to save the last readings: {e:#}; trying again each minute"
                    );
                }
                saving.failing = true;
            }
        });
        // Only a panic in the save fails it, and that has said so.
        let _ = written.await;
    }

    /// Works out which pair an advertisement is from, and decrypts it with
    /// that pair's ENC_KEY. Returns the pair's real MAC when identified.
    ///
    /// The address must resolve with a stored IRK, and only that pair's
    /// ENC_KEY is tried, its plaintext passing the suffix check (#134). An
    /// address no stored IRK resolves counts only for a pair with no IRK yet,
    /// by its ENC_KEY alone; anything else is dropped. Why both checks:
    /// `docs/app/identification.md#why-both-checks`.
    pub(crate) async fn identify_and_decrypt(
        &self,
        data: &mut ProximityData,
        ble_mac: Address,
    ) -> Option<Address> {
        let encrypted = data.encrypted_portion()?.to_vec();
        let keys = self.inner.read().await.keys.clone();

        if let Some(real_mac) = self.resolve(ble_mac, &keys, Instant::now()) {
            let decrypted = decrypt_for_device(&encrypted, &keys[&real_mac].enc_key, real_mac);
            if let Ok(decrypted) = decrypted
                && data.add_decrypted_data(&decrypted).is_ok()
            {
                tracing::debug!("BLE decryptable: {ble_mac} -> {real_mac} (address resolved)");
                return Some(real_mac);
            }
            tracing::debug!(
                "BLE not decryptable: {ble_mac} resolves to {real_mac}, whose key does not \
                 decrypt it"
            );
            return None;
        }

        // Pairs with no IRK yet, by their ENC_KEY alone.
        let without_irk = keys.iter().filter(|(_, k)| k.irk.is_none());
        for (&real_mac, pair) in without_irk {
            let Ok(decrypted) = decrypt_for_device(&encrypted, &pair.enc_key, real_mac) else {
                continue;
            };
            if data.add_decrypted_data(&decrypted).is_ok() {
                tracing::debug!("BLE decryptable: {ble_mac} -> {real_mac} (key matched, no IRK)");
                return Some(real_mac);
            }
        }

        if !keys.is_empty() {
            tracing::debug!("BLE not decryptable: {ble_mac} (resolves with no stored IRK)");
        }
        None
    }

    /// The pair whose stored IRK resolves `ble_mac`, if one does.
    ///
    /// Remembered in [`ResolvedAddresses`], so that an address costs one
    /// resolve - an AES block per stored IRK - rather than one per
    /// advertisement. Only an address that resolved is remembered, with the
    /// IRK that resolved it, so a pair re-paired since is not taken for it.
    /// One no IRK resolves is resolved again with each advertisement:
    /// remembered, a spammer rotating its address would churn the map
    /// (`docs/app/identification.md#resolved-addresses`).
    fn resolve(
        &self,
        ble_mac: Address,
        keys: &HashMap<Address, PairKeys>,
        now: Instant,
    ) -> Option<Address> {
        if !rpa::is_resolvable(ble_mac) {
            return None;
        }
        let known = self.resolved_addresses().get(ble_mac, now);
        if let Some((real_mac, irk)) = known
            && keys.get(&real_mac).is_some_and(|k| k.irk == Some(irk))
        {
            return Some(real_mac);
        }
        // Two IRKs resolving one address is a 2^-24 chance; the first is
        // taken, and its ENC_KEY decides.
        let (real_mac, irk) = keys.iter().find_map(|(&mac, k)| {
            k.irk
                .filter(|irk| rpa::resolves(irk, ble_mac))
                .map(|irk| (mac, irk))
        })?;
        self.resolved_addresses()
            .insert(ble_mac, real_mac, irk, now);
        Some(real_mac)
    }

    /// The resolved addresses - see `Coordinator::resolved`.
    fn resolved_addresses(&self) -> std::sync::MutexGuard<'_, ResolvedAddresses> {
        self.resolved.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Handles one BLE advertisement.
    ///
    /// One that is not identified as a pair's - see
    /// [`Coordinator::identify_and_decrypt`] - is dropped here: see the module
    /// docs. The scanner's `BLE parsable:` log still records it.
    ///
    /// An AAP connection only supersedes BLE *for that one device*. Other AirPods
    /// keep advertising and must still be tracked, so the decision is made per
    /// device after identification rather than globally.
    pub async fn handle_advertisement(&self, mut data: ProximityData, ble_mac: Address) {
        let Some(real_mac) = self.identify_and_decrypt(&mut data, ble_mac).await else {
            return;
        };

        let identity = Identity {
            product_id: data.product_id,
            color: data.color,
        };
        let mut state = PodState {
            source: DataSource::Ble,
            left_battery: data.left_battery,
            right_battery: data.right_battery,
            case_battery: data.case_battery,
            left_charging: data.left_charging,
            right_charging: data.right_charging,
            case_charging: data.case_charging,
            left_in_ear: Some(data.left_in_ear),
            right_in_ear: Some(data.right_in_ear),
            lid_open: data.lid_open,
            connection_state: Some(data.connection_state),
            // BLE carries no noise control mode; `snapshot` attaches the one
            // reported over AAP for the connected device. Nor Conversation
            // Awareness, nor its levels.
            noise_mode: None,
            conversation_awareness: None,
            speech_level: None,
            name: None,
            identity: Some(identity),
            current_ble_mac: Some(ble_mac),
            restored: false,
            // Stamped from the entry in `snapshot`.
            last_seen: None,
        };

        tracing::debug!(
            "BLE {ble_mac} [decrypted]: left={:?} right={:?} case={:?} lid_open={:?} one_in_case={} in_ear={}/{}",
            state.left_battery,
            state.right_battery,
            state.case_battery,
            state.lid_open,
            data.one_in_case,
            data.left_in_ear,
            data.right_in_ear
        );
        tracing::debug!("BLE {ble_mac} raw: {}", crate::hex(&data.raw_data));

        let mut inner = self.inner.write().await;
        let now = SystemTime::now();
        // A worn pod whose partner is in the case has no lid to report, and
        // both advertise: replaced wholesale, the lid would blink between the
        // partner's reading and none as the two pods' advertisements came. So
        // it keeps the lid the device last reported.
        if data.lid_open.is_none() && data.one_in_case {
            state.lid_open = inner
                .ble
                .get(&real_mac)
                .filter(|e| age(e.last_seen, now).is_some_and(|age| age < BLE_CACHE_TTL))
                .and_then(|e| e.state.lid_open);
        }
        let shown = inner.linked_live(real_mac);
        // Recorded even while AAP supersedes it, so that the reading is
        // current the moment the link drops - and a linked device's
        // snapshot takes its live fields from it meanwhile.
        inner.ble.insert(
            real_mac,
            Entry {
                state,
                last_seen: now,
            },
        );
        let pruned = inner.prune(now);
        let identified = inner
            .links
            .get_mut(&real_mac)
            .is_some_and(|link| link.identity.replace(identity) != Some(identity));
        // Judged by what the snapshot shows, not by the advertisement: once
        // the link reports in-ear, an advertisement's is not on show, and
        // two from the two pods differed within 0.6 s with both worn (#142).
        let live_changed = inner.linked_live(real_mac) != shown;
        // AAP data for this device is exact and current; its own
        // advertisements lag behind. Nothing a snapshot shows has changed -
        // unless they told the link something new about the device,
        // typically its model on the first advertisement after connecting;
        // or changed what the snapshot takes from them, such as a pod taken
        // out of an ear before the link has reported one; or the prune
        // dropped something.
        if inner.supersedes_ble(real_mac) && !identified && !live_changed && !pruned {
            return;
        }
        self.broadcast(&inner);
    }

    /// Opens a device's AAP link and reads it in a task of its own until it
    /// ends, and nothing opens it again: for this module's tests, which have
    /// no supervisor. The app's links are each opened and run by their pair's
    /// supervisor, in `attachment`.
    #[cfg(all(test, feature = "harness"))]
    async fn connect_aap(self: &Arc<Self>, mac_addr: Address) -> Result<()> {
        let link = self.open_aap(mac_addr).await?;
        tokio::spawn(link.run());
        Ok(())
    }

    /// Opens an AAP session to one device - see [`aap::Client::open`] - which
    /// makes it the device's link, for [`AapLink::run`] to read. Other
    /// devices' links are untouched. Fails when the device has a link already,
    /// or gets one meanwhile: that one stands.
    ///
    /// Dropped before it is done - timed out, or the pair detached - it leaves
    /// no link behind: a connection that is up closes with it, one still
    /// connecting closes once it is up (see [`aap::Client::connect`]), and the
    /// link is recorded only at the last step, which nothing awaits after.
    pub(crate) async fn open_aap(self: &Arc<Self>, mac_addr: Address) -> Result<AapLink> {
        anyhow::ensure!(
            !self.inner.read().await.links.contains_key(&mac_addr),
            "AAP to {mac_addr} is up already"
        );

        let client = Arc::new(self.aap_client(mac_addr).open().await?);
        // Ask for the proximity keys on every connection, not only when none is
        // stored: what would change them is not known (a re-pairing kept them,
        // `docs/protocol/aap/keys.md`), and a stale IRK or key leaves the pair
        // unidentified, so it would silently vanish from BLE. The reply is
        // stored by `aap_read_loop` like any key packet, and waits in the socket
        // until that loop starts. Without a key only BLE suffers, so a failure
        // does not fail the connection.
        if let Err(e) = client.request_proximity_keys().await {
            tracing::warn!("failed to request encryption keys: {e:#}");
        }
        let link = {
            let mut inner = self.inner.write().await;
            // Connecting takes a while, and another attempt may have got there
            // first. Its link stands; this one goes.
            if inner.links.contains_key(&mac_addr) {
                client.shutdown();
                anyhow::bail!("AAP to {mac_addr} came up meanwhile; dropping the duplicate");
            }
            // Advertised before the link came up, if the cache still has it.
            let identity = inner.ble.get(&mac_addr).and_then(|e| e.state.identity);
            inner.links.insert(
                mac_addr,
                Link {
                    client: client.clone(),
                    reading: None,
                    // Reported by the startup dump, once the read loop is up.
                    noise_mode: ModeState::default(),
                    conversation_awareness: None,
                    speech_level: None,
                    // Reported by the startup dump too.
                    name: None,
                    // Reported as the link comes up, waiting in the socket.
                    ear: EarState::with_hold(self.role_switch_hold),
                    identity,
                    since: Instant::now(),
                },
            );
            let (commands, queue) = mpsc::unbounded_channel();
            self.queues().insert(
                mac_addr,
                LinkQueue {
                    client: client.clone(),
                    commands,
                },
            );
            // Ahead of the link's levels and places, which are told under
            // this lock too.
            self.tell_speech(SpeechEvent::LinkUp { mac: mac_addr });
            self.tell_ears(EarEvent::LinkUp { mac: mac_addr });
            self.broadcast(&inner);
            AapLink {
                coordinator: self.clone(),
                mac: mac_addr,
                client,
                queue,
                ended: false,
            }
        };

        tracing::info!(
            "AAP connected to {mac_addr} - exact battery for this device; \
             devices without a link continue over BLE"
        );
        Ok(link)
    }

    /// A client for a new link to `mac_addr`: to the device over L2CAP, or under
    /// the harness to the fake's socket, as decided at construction.
    fn aap_client(&self, mac_addr: Address) -> aap::Client {
        #[cfg(feature = "harness")]
        if let Some(dir) = &self.aap_socket_dir {
            let path = aap::client::unix_socket_path(dir, mac_addr);
            return aap::Client::with_unix_socket(mac_addr, path);
        }
        aap::Client::new(mac_addr)
    }

    /// The links' command queues - see `Coordinator::queues`.
    fn queues(&self) -> std::sync::MutexGuard<'_, HashMap<Address, LinkQueue>> {
        // Nothing panics while holding it, but a poisoned map is as good as any.
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes one device's AAP link. Any other device's link is untouched.
    pub(crate) async fn disconnect_aap(&self, mac_addr: Address) {
        self.drop_link(mac_addr, None).await;
    }

    /// Removes a link, broadcasting and returning true if there was one. With
    /// `only`, the link goes only while it is still that client's: a read loop
    /// reporting its socket dead must not take down a newer link to the device.
    async fn drop_link(&self, mac_addr: Address, only: Option<&Arc<aap::Client>>) -> bool {
        {
            let mut inner = self.inner.write().await;
            match inner.links.get(&mac_addr) {
                Some(link) if only.is_none_or(|c| Arc::ptr_eq(&link.client, c)) => {}
                _ => return false,
            }
            // After the link goes its readings are no longer current; kept, they
            // left the UI reporting "Source: AAP" for a disconnected device. The
            // cached advertisement shows through in their place.
            if let Some(link) = inner.links.remove(&mac_addr) {
                // Wakes the read loop parked in recv so it can exit and drop its Arc.
                link.client.shutdown();
                // The link's own queue, as the two go together, matched by
                // client as the record was. A command sent from here on fails
                // at once, and one still queued fails as the run ends, its
                // client shut.
                let mut queues = self.queues();
                if queues
                    .get(&mac_addr)
                    .is_some_and(|queue| Arc::ptr_eq(&queue.client, &link.client))
                {
                    queues.remove(&mac_addr);
                }
                drop(queues);
                // After every level and place the link brought, as those
                // were told under this lock too.
                self.tell_speech(SpeechEvent::LinkGone { mac: mac_addr });
                self.tell_ears(EarEvent::LinkGone { mac: mac_addr });
            }
            // Without this the UI, tray, attachment and notifier never hear
            // that the connection ended and keep showing the last AAP reading.
            self.broadcast(&inner);
        }
        tracing::info!("AAP disconnected from {mac_addr} - showing its last BLE advertisement");
        true
    }

    /// Reads one link's packets until a read fails, and returns that error.
    /// It is handed the client rather than looking it up, so it only ever
    /// hears its own device.
    ///
    /// While an ear report is held for a role switch - see [`EarState`] - a
    /// read waits no longer than the hold, and the report is taken when it is
    /// over. The read given up on loses nothing: the socket gives up a packet
    /// only to a read that returns it.
    async fn aap_read_loop(&self, mac_addr: Address, client: &Arc<aap::Client>) -> anyhow::Error {
        tracing::debug!("AAP read loop started for {mac_addr}");
        let mut held_until: Option<Instant> = None;
        loop {
            let read = client.read_packet();
            let read = match held_until {
                Some(until) => match tokio::time::timeout_at(until.into(), read).await {
                    Ok(read) => read,
                    Err(_) => {
                        held_until = self.release_ear(mac_addr, client, Instant::now()).await;
                        continue;
                    }
                },
                None => read.await,
            };
            let packet = match read {
                Ok(p) => p,
                Err(e) => return e,
            };

            // Redacted, as a key packet or the device information (`0x1D`,
            // serial numbers) may be among them: logs get pasted into issues.
            tracing::debug!(
                "AAP packet ({} bytes): {}",
                packet.len(),
                aap::keys::redacted_hex(&packet[..packet.len().min(8)])
            );
            // Whole, for what lies past the first 8 bytes: 0x08's last two
            // took a local build to see (#142).
            tracing::trace!(
                "AAP packet ({} bytes), whole: {}",
                packet.len(),
                aap::keys::redacted_hex(&packet)
            );

            match aap::decode(&packet) {
                Ok(Message::Battery(info)) => {
                    self.handle_battery_info(info, mac_addr, client).await;
                }
                // Each ends the hold on a report held for a role switch, if
                // one is: a report replaces it, the primary pod reads it.
                Ok(Message::Ear(report)) => {
                    held_until = self.handle_ear_report(report, mac_addr, client).await;
                }
                Ok(Message::Primary(primary)) => {
                    self.handle_primary(primary, mac_addr, client).await;
                    held_until = None;
                }
                // The device's own report: its startup dump, or a mode changed
                // from another device such as an iPhone.
                Ok(Message::NoiseMode(mode)) => {
                    self.handle_noise_mode(mode, mac_addr, client).await;
                }
                Ok(Message::Keys(keys)) => match aap::find_encryption_key(&keys) {
                    Ok(enc_key) => {
                        let irk = aap::find_irk(&keys)
                            .inspect_err(|e| tracing::warn!("AAP key packet's IRK not kept: {e}"))
                            .ok();
                        self.store_keys(mac_addr, &enc_key, irk).await;
                    }
                    Err(e) => tracing::warn!("AAP key packet rejected: {e}"),
                },
                // The startup dump's, or a change made elsewhere, should the
                // pods report one.
                Ok(Message::ConversationAwareness(enabled)) => {
                    self.handle_conversation_awareness(enabled, mac_addr, client)
                        .await;
                }
                Ok(Message::SpeechLevel(level)) => {
                    self.handle_speech_level(level, mac_addr, client).await;
                }
                Ok(Message::DeviceInfo(info)) => {
                    self.handle_device_info(info, mac_addr, client).await;
                }
                Ok(Message::Unknown { .. }) => {}
                Err(e) => tracing::warn!("AAP {e}"),
            }
        }
    }

    /// Makes a battery packet the link's reading, and broadcasts.
    ///
    /// A battery packet carries battery only. [`Inner::snapshot`] merges in
    /// the rest: the identity the link holds, the mode, in-ear from the link's
    /// ear reports, and what the cached advertisement says about lid and
    /// connection state - and about ears, until the link has reported them.
    async fn handle_battery_info(
        &self,
        info: aap::BatteryInfo,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        let state = PodState {
            source: DataSource::Aap,
            left_battery: info.left.and_then(|b| b.level),
            right_battery: info.right.and_then(|b| b.level),
            case_battery: info.case.and_then(|b| b.level),
            left_charging: info.left.is_some_and(|b| b.is_charging()),
            right_charging: info.right.is_some_and(|b| b.is_charging()),
            case_charging: info.case.is_some_and(|b| b.is_charging()),
            // Not in a battery packet; merged in by `snapshot` where known.
            left_in_ear: None,
            right_in_ear: None,
            lid_open: None,
            connection_state: None,
            noise_mode: None,
            conversation_awareness: None,
            speech_level: None,
            name: None,
            identity: None,
            // AAP knows only the real address, and is always live.
            current_ble_mac: None,
            restored: false,
            last_seen: None,
        };

        tracing::debug!(
            "AAP battery: left={:?} right={:?} case={:?}",
            state.left_battery,
            state.right_battery,
            state.case_battery
        );

        let mut inner = self.inner.write().await;
        // A packet read just before a disconnect must not bring back AAP
        // state for a link that is gone, nor land on a newer one.
        let Some(link) = inner.live_link(mac_addr, client) else {
            return;
        };
        link.reading = Some(state);
        self.broadcast(&inner);
    }

    /// Records an ear report from `client`'s link - see [`EarState`] - and
    /// broadcasts if that changed what the snapshot shows. Returns when it is
    /// to be taken if it is held for a role switch.
    async fn handle_ear_report(
        &self,
        report: EarReport,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) -> Option<Instant> {
        tracing::debug!(
            "AAP ear report: primary pod {:?}, secondary {:?}",
            report.primary,
            report.secondary
        );
        let held_until = self
            .update_ear(mac_addr, client, |ear| ear.report(report, Instant::now()))
            .await;
        if held_until.is_some() {
            tracing::debug!("AAP ear report held for the primary pod: a role switch, it seems");
        }
        held_until
    }

    /// Records the primary pod `client`'s link reported - see [`EarState`] -
    /// and broadcasts if that changed what the snapshot shows.
    async fn handle_primary(
        &self,
        primary: Option<PodSide>,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        tracing::debug!("AAP primary pod: {primary:?}");
        self.update_ear(mac_addr, client, |ear| ear.set_primary(primary))
            .await;
    }

    /// Takes the ear report held for a role switch on `client`'s link, its
    /// hold over by `now` with no primary pod packet, and broadcasts if that
    /// changed what the snapshot shows. Returns when it is to be taken if it
    /// is still held.
    async fn release_ear(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        now: Instant,
    ) -> Option<Instant> {
        let mut released = false;
        let held_until = self
            .update_ear(mac_addr, client, |ear| released = ear.release(now))
            .await;
        if released {
            tracing::debug!("AAP ear report held for a role switch taken: no primary pod came");
        }
        held_until
    }

    /// Changes the ear state of `client`'s link, and broadcasts if that
    /// changed what the snapshot shows - under the lock it was changed under
    /// (#88). Nothing it shows before the link's first battery packet: that
    /// broadcasts with the ear state in. Tells the consumers of
    /// [`Coordinator::ear_events`] where the pods are if that changed,
    /// battery packet or not. Returns when the report held for a role switch
    /// is to be taken, if one is.
    async fn update_ear(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        change: impl FnOnce(&mut EarState),
    ) -> Option<Instant> {
        let mut inner = self.inner.write().await;
        let shown = inner.linked_live(mac_addr);
        // A packet read just before a disconnect must not land on a newer
        // link.
        let link = inner.live_link(mac_addr, client)?;
        let places = link.ear.places();
        change(&mut link.ear);
        let held_until = link.ear.held_until();
        // Under the lock, as the link's going is told: never after it. And
        // before the snapshot, so whoever sees the pods' new places there
        // finds the event queued already.
        if let Some((left, right)) = link.ear.places().filter(|&now| Some(now) != places) {
            self.tell_ears(EarEvent::Places {
                mac: mac_addr,
                left,
                right,
            });
        }
        if inner.linked_live(mac_addr) != shown {
            self.broadcast(&inner);
        }
        held_until
    }

    /// Records a mode the device reported on `client`'s link, and broadcasts if
    /// it changed.
    ///
    /// The startup dump repeats, and an unchanged mode is not worth waking the
    /// window, tray, attachment and notifier for. A stale report of the mode a
    /// command just replaced is not taken - see [`ModeState`].
    async fn handle_noise_mode(
        &self,
        mode: aap::NoiseMode,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        {
            let mut inner = self.inner.write().await;
            match inner.report_noise_mode(mac_addr, client, mode) {
                Some(Report::Changed) => self.broadcast(&inner),
                Some(Report::Stale) => {
                    tracing::info!(
                        "Noise control mode reported by {mac_addr}: {mode}, the one just \
                         replaced; dropped as stale"
                    );
                    return;
                }
                Some(Report::Unchanged) | None => return,
            }
        }
        tracing::info!("Noise control mode reported by {mac_addr}: {mode}");
    }

    /// Records whether Conversation Awareness is on, as the device reported
    /// it on `client`'s link, and broadcasts if that changed it. The startup
    /// dump reports it; whether a change made on another device is reported
    /// is not known.
    async fn handle_conversation_awareness(
        &self,
        enabled: bool,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        {
            let mut inner = self.inner.write().await;
            // A packet read just before a disconnect must not land on a
            // newer link.
            let Some(link) = inner.live_link(mac_addr, client) else {
                return;
            };
            if link.conversation_awareness.replace(enabled) == Some(enabled) {
                return;
            }
            self.broadcast(&inner);
        }
        tracing::info!(
            "Conversation Awareness reported by {mac_addr}: {}",
            on_off(enabled)
        );
    }

    /// Takes the device information reported on `client`'s link, in the
    /// startup dump: the pods' name onto the link, and the firmware version
    /// into those kept for the run. Broadcasts if that changed either. A
    /// field that did not read is logged and passed over, and the other
    /// taken all the same. Only the two are logged: the packet they came in
    /// holds serial numbers.
    async fn handle_device_info(
        &self,
        info: DeviceInfo,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        let DeviceInfo { name, version } = info;
        let name = name
            .inspect_err(|e| tracing::warn!("Name reported by {mac_addr} not read: {e}"))
            .ok();
        let version = version
            .inspect_err(|e| tracing::warn!("Firmware reported by {mac_addr} not read: {e}"))
            .ok();
        let renamed = {
            let mut inner = self.inner.write().await;
            // A packet read just before a disconnect must not land on a
            // newer link, nor its version over the newer link's.
            let Some(link) = inner.live_link(mac_addr, client) else {
                return;
            };
            let renamed = name.is_some() && link.name != name;
            if renamed {
                link.name.clone_from(&name);
            }
            let updated = version.is_some_and(|version| {
                inner.firmware_versions.insert(mac_addr, version) != Some(version)
            });
            if renamed || updated {
                self.broadcast(&inner);
            }
            renamed
        };
        if let Some(name) = name.filter(|_| renamed) {
            tracing::info!("Name reported by {mac_addr}: {name:?}");
        }
        // Field 10 as sent, beside our reading of it, which one value alone
        // confirms: a wrong reading shows in the log.
        if let Some(version) = version {
            tracing::info!(
                "Firmware reported by {mac_addr}: {version} (Marketing Version {})",
                version.raw()
            );
        }
    }

    /// Records a speech level from `client`'s link, broadcasts if it is
    /// another than the last, and tells it to the consumers of
    /// [`Coordinator::speech_events`] - what lowers the volume while the
    /// user speaks - another or not.
    async fn handle_speech_level(&self, level: u8, mac_addr: Address, client: &Arc<aap::Client>) {
        tracing::debug!(
            "AAP speech level {}",
            aap::conversation::describe_level(level)
        );
        let mut inner = self.inner.write().await;
        let Some(link) = inner.live_link(mac_addr, client) else {
            return;
        };
        if link.speech_level.replace(level) != Some(level) {
            self.broadcast(&inner);
        }
        // Under the lock, as the link's going is told: never after it.
        self.tell_speech(SpeechEvent::Level {
            mac: mac_addr,
            level,
        });
    }

    /// Sends a command to one device, over its AAP link, and resolves to how
    /// it went: every command goes this way, the window's and the tray's, and
    /// every caller hears whether it was done (#54).
    ///
    /// The command takes its place in the link's queue here, in the call,
    /// not when the future is first polled: the link carries out its
    /// commands one at a time, in the order of the calls, however their
    /// futures are awaited or spawned. Spawned apart, two quick switches went
    /// out in either order, and each recorded its mode after its own send,
    /// so the mode on show could be the one sent first. Nor does sending
    /// wait for anything: it can be called from the GTK main context, whose
    /// thread no tokio runtime runs on, and the future awaited there.
    ///
    /// Fails at once for a name `aap::name::check` refuses, without a link, or
    /// with a link going; and once queued, when the send fails, or the link
    /// goes before the command is sent. All of those are a command not
    /// carried out. One the link went in the middle of, which may or may not
    /// have reached the device, fails with [`ConnectionLost`] instead.
    pub fn command(
        &self,
        mac_addr: Address,
        command: Command,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let (done, result) = oneshot::channel();
        let what = command.to_string();
        // A name the pods would not take is refused here, never queued.
        let refused = match &command {
            Command::Rename(name) => aap::name::check(name).err(),
            _ => None,
        };
        let queued = match (refused, self.queues().get(&mac_addr)) {
            (Some(refused), _) => Err(refused.into()),
            (None, Some(queue)) => queue
                .commands
                .send(Queued { command, done })
                .map_err(|_| anyhow::anyhow!("AAP to {mac_addr} is closing")),
            (None, None) => Err(anyhow::anyhow!("no AAP connection to {mac_addr}")),
        };
        async move {
            queued.with_context(|| format!("{what} not sent"))?;
            // Dropped unanswered when the link went with the command still
            // in its queue; one under way answers for itself.
            result.await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "AAP to {mac_addr} went before the {what} was sent"
                ))
            })
        }
    }

    /// Carries out the commands queued for one link, each in turn, and tells
    /// each one's caller how it went. Never ends of itself: the queue closes
    /// with the link's record, whose client is shut then, and the read loop
    /// ends the run.
    async fn command_loop(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        queue: &mut mpsc::UnboundedReceiver<Queued>,
    ) -> Infallible {
        while let Some(Queued { command, done }) = queue.recv().await {
            // Answers for the command if the run is dropped while it is
            // under way.
            let mut underway = Underway {
                mac: mac_addr,
                command: command.clone(),
                done: Some(done),
            };
            let result = self.carry_out(mac_addr, client, command).await;
            underway.finish(result);
        }
        std::future::pending().await
    }

    /// Sends one command on `client`'s link.
    async fn carry_out(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        command: Command,
    ) -> Result<()> {
        match command {
            Command::NoiseControl(mode) => self.set_noise_control(mac_addr, client, mode).await,
            Command::ConversationAwareness(enabled) => {
                self.set_conversation_awareness(mac_addr, client, enabled)
                    .await
            }
            Command::Rename(name) => self.rename(mac_addr, client, name).await,
        }
    }

    /// Switches one connected device's noise control mode.
    ///
    /// The new mode is recorded optimistically. Nothing the device answers
    /// confirms it on every model: AirPods 4 sent the 0x0D echo carrying the
    /// mode back for one change in four; AirPods Pro 3 echo every change, but
    /// only about a second later. With Conversation Awareness on, its levels
    /// 08 and 09 come around the change, naming no mode. Waiting would leave
    /// the interface on the old mode meanwhile, or for good. A later report
    /// confirms what we set - or,
    /// when it is of the mode replaced and comes within
    /// [`aap::noise::STALE_REPORT_WINDOW`] before the mode set is reported, is
    /// dropped as stale (#113).
    ///
    /// The link's commands go one at a time, so the mode recorded last is the
    /// one sent last.
    async fn set_noise_control(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        mode: aap::NoiseMode,
    ) -> Result<()> {
        // A Pro 3 and a Pro 2 (USB-C) refused a bare Off with an error chime;
        // which firmware brought that is not known. Allowing Off first lets it
        // through. That the pods keep it allowed beyond this session is a guess,
        // so it is sent only when Off is what was asked for, never at every
        // connection
        // (`docs/protocol/aap/noise-control.md#off-has-a-prerequisite-of-its-own`,
        // `docs/app/noise-control.md#allowing-off`).
        if mode == aap::NoiseMode::Off {
            client
                .set_allow_off_listening_mode(true)
                .await
                .context("failed to allow Off as a listening mode")?;
        }
        client.set_noise_mode(mode).await?;
        tracing::info!("Noise control of {mac_addr} set to {mode}");

        let mut inner = self.inner.write().await;
        // On the link the command went out on, and only while it lasts. One
        // that dropped and came back meanwhile is a new session, and the
        // device reports its mode there itself.
        if inner.record_noise_mode(mac_addr, client, mode) {
            self.broadcast(&inner);
        }
        Ok(())
    }

    /// Turns one connected device's Conversation Awareness on or off, with
    /// LibrePods' packet, which a Pro 3 took from the window (2026-11-02,
    /// #150).
    ///
    /// Recorded once the packet is away, as the mode is: the pods are not
    /// known to report the setting again when it changes - LibrePods says
    /// they do not - so nothing would ever confirm it. Should one come, it
    /// is taken as any report is. A change made on another device meanwhile
    /// may never be reported, and the setting on show can drift from the
    /// pods' until the next connection's startup dump; that is accepted.
    async fn set_conversation_awareness(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        enabled: bool,
    ) -> Result<()> {
        client.set_conversation_awareness(enabled).await?;
        tracing::info!(
            "Conversation Awareness of {mac_addr} turned {}",
            on_off(enabled)
        );

        let mut inner = self.inner.write().await;
        // On the link the command went out on, and only while it lasts.
        let Some(link) = inner.live_link(mac_addr, client) else {
            return Ok(());
        };
        if link.conversation_awareness.replace(enabled) != Some(enabled) {
            self.broadcast(&inner);
        }
        Ok(())
    }

    /// Renames one connected pair: the name the pods give every host. BlueZ's
    /// alias is never touched - set, it would pin the name on this computer
    /// so that a later rename elsewhere never showed here; cleared, it would
    /// throw away a name the user gave in GNOME Settings (#160).
    ///
    /// Recorded once the packet is away, as the mode is: the pods answer
    /// nothing, and take the name at once - the next link's startup dump
    /// carries it. BlueZ's `Name` follows when the pair next connects.
    async fn rename(
        &self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        name: String,
    ) -> Result<()> {
        client.rename(&name).await?;
        tracing::info!("Renamed {mac_addr} to {name:?}");

        let mut inner = self.inner.write().await;
        // On the link the command went out on, and only while it lasts.
        let Some(link) = inner.live_link(mac_addr, client) else {
            return Ok(());
        };
        if link.name.as_ref() != Some(&name) {
            link.name = Some(name);
            self.broadcast(&inner);
        }
        Ok(())
    }

    /// Takes a newly received ENC_KEY and IRK into use, broadcasts the new key
    /// list, and persists them.
    ///
    /// Keys are requested on every connection, so these are usually the keys
    /// already held and already saved. That is skipped rather than rewriting the
    /// keystore each time - which also keeps a device repeating its key packet
    /// from keeping the disk busy. "Already saved" goes by the last save that
    /// succeeded, so a key whose save failed is written again when it next
    /// arrives.
    ///
    /// A packet that brings no usable IRK keeps the one stored, as long as the
    /// ENC_KEY is the one stored with it. With a new ENC_KEY the old IRK goes
    /// too, as it may resolve none of the pair's new addresses: without one,
    /// the pair is identified by its ENC_KEY alone rather than not at all
    /// (`docs/app/identification.md#pairs-with-no-irk`).
    async fn store_keys(
        &self,
        mac_addr: Address,
        enc_key: &[u8; KEY_LEN],
        irk: Option<[u8; KEY_LEN]>,
    ) {
        {
            let mut inner = self.inner.write().await;
            let previous = inner.keys.get(&mac_addr);
            let irk = irk.or_else(|| {
                previous
                    .filter(|p| p.enc_key == enc_key)
                    .and_then(|p| p.irk)
            });
            let keys = PairKeys::new(*enc_key, irk);
            // Identifying from here on, whether or not the keys reach the disk.
            if previous != Some(&keys) {
                inner.keys.insert(mac_addr, keys);
                self.broadcast(&inner);
            }
        }

        // Owned, so that it can go with the save, which then holds it until
        // the save is done and recorded, whatever becomes of this call. The
        // read loop calling it is dropped wherever it is when its pair
        // detaches: the drop let go of the lock while the save went on, and
        // the next save ran alongside it, the older keys landing last.
        let mut persistence = self.keystore.clone().lock_owned().await;
        let Some(keystore) = persistence.keystore.clone() else {
            tracing::debug!("Keys of {mac_addr} kept for this session only");
            return;
        };
        let keys = self.inner.read().await.keys.clone();
        if keys == persistence.saved {
            tracing::debug!("Keys of {mac_addr} already saved");
            return;
        }

        #[cfg(test)]
        let hook = self.before_save.lock().unwrap().take();
        // Off the async workers: the save waits on the disk. A blocking task
        // runs to its end however its awaiter fares.
        let saved = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            let saved = keystore.save(&keys);
            if saved.is_ok() {
                persistence.saved = keys;
            }
            saved
        })
        .await
        .unwrap_or_else(|e| Err(e.into()));
        match saved {
            Ok(()) => tracing::info!("Stored the keys of {mac_addr}"),
            Err(e) => tracing::warn!("failed to save the keys to disk: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::ear::Place;
    use crate::aap::info::FieldError;
    use crate::ble::decrypt::random_key;

    fn entry(age: Duration) -> Entry {
        Entry {
            state: PodState::empty(DataSource::Ble),
            last_seen: SystemTime::now() - age,
        }
    }

    fn inner_with(devices: Vec<(Address, Duration)>) -> Inner {
        Inner {
            ble: devices
                .into_iter()
                .map(|(m, age)| (m, entry(age)))
                .collect(),
            links: HashMap::new(),
            keys: HashMap::new(),
            device_names: HashMap::new(),
            firmware_versions: HashMap::new(),
        }
    }

    /// A link as `open_aap` leaves it. The client never connects; building
    /// one does no I/O, and which address it holds does not matter here.
    fn link(reading: Option<PodState>) -> Link {
        Link {
            client: Arc::new(aap::Client::new(Address::any())),
            reading,
            noise_mode: ModeState::default(),
            conversation_awareness: None,
            speech_level: None,
            name: None,
            ear: EarState::default(),
            identity: None,
            since: Instant::now(),
        }
    }

    /// The client of the device's link.
    async fn client_of(coordinator: &Coordinator, mac: Address) -> Arc<aap::Client> {
        coordinator.inner.read().await.links[&mac].client.clone()
    }

    fn coordinator_with(inner: Inner) -> (Arc<Coordinator>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (updates, _) = watch::channel(inner.snapshot());
        let coordinator = Arc::new(Coordinator {
            inner: RwLock::new(inner),
            queues: std::sync::Mutex::default(),
            resolved: std::sync::Mutex::default(),
            speech: std::sync::Mutex::default(),
            ears: std::sync::Mutex::default(),
            keystore: Arc::new(Mutex::new(Persistence {
                keystore: Some(keystore_in(&dir)),
                saved: HashMap::new(),
            })),
            readings: Arc::new(Mutex::new(SavedReadings {
                store: Some(readings::Store::with_dir(dir.path().to_path_buf())),
                saved: HashMap::new(),
                failing: false,
            })),
            updates,
            #[cfg(feature = "harness")]
            aap_socket_dir: None,
            role_switch_hold: ROLE_SWITCH_HOLD,
            before_send: std::sync::Mutex::default(),
            before_save: std::sync::Mutex::default(),
        });
        (coordinator, dir)
    }

    /// Synthetic, like every key in these tests.
    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    const KEY: [u8; 16] = [7; 16];

    /// Devices for tests where nothing but telling them apart matters.
    const AA: Address = Address::new([0xAA; 6]);
    const BB: Address = Address::new([0xBB; 6]);
    const CC: Address = Address::new([0xCC; 6]);
    const DD: Address = Address::new([0xDD; 6]);

    /// The randomized address an advertisement arrives from.
    const BLE_MAC: Address = Address::new([0x5C, 0x4D, 0x3F, 0xB5, 0x41, 0xB6]);

    fn keystore_in(dir: &tempfile::TempDir) -> Keystore {
        Keystore::with_dir(dir.path().to_path_buf()).unwrap()
    }

    fn chmod(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Regression (#38): a key file that did not load was overwritten by the
    /// next key to arrive, holding that key alone - every other one was lost.
    #[tokio::test]
    async fn a_key_file_that_did_not_load_is_set_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        // One entry that does not decode fails the whole file, the good key with it.
        let corrupt = r#"{"version":1,"keys":{
            "11:22:33:44:55:66":"AQIDBAUGBwgJCgsMDQ4PEA==",
            "AA:BB:CC:DD:EE:FF":"not base64!"}}"#;
        std::fs::write(dir.path().join("keys.json"), corrupt).unwrap();

        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));
        assert_eq!(coordinator.inner.read().await.keys.len(), 0);
        coordinator.store_keys(MAC, &KEY, None).await;

        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_str().unwrap().contains("keys.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), corrupt);
        assert_eq!(
            keystore_in(&dir).load().unwrap(),
            HashMap::from([(MAC, PairKeys::new(KEY, None))]),
            "a fresh file in its place"
        );
    }

    /// A key file that can be neither read nor moved aside is never saved over.
    /// The keys that arrive are still used, for this session only.
    #[tokio::test]
    async fn a_key_file_that_cannot_be_set_aside_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        std::fs::write(&path, "original").unwrap();
        let keystore = keystore_in(&dir);

        chmod(&path, 0o000);
        chmod(dir.path(), 0o500);
        let coordinator = Coordinator::with_keystore(Some(keystore));
        // Writable again, so only the coordinator stands between the next key
        // and the file.
        chmod(dir.path(), 0o700);
        coordinator.store_keys(MAC, &KEY, None).await;
        chmod(&path, 0o600);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(coordinator.inner.read().await.keys.len(), 1);
    }

    /// Regression (#38): "unchanged" was judged against memory, which already
    /// held the key whose save had failed, so the save was never retried.
    #[tokio::test]
    async fn a_failed_save_is_retried_when_the_key_arrives_again() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));

        // Read-only, so the save cannot create its temporary file.
        chmod(dir.path(), 0o500);
        coordinator.store_keys(MAC, &KEY, None).await;
        chmod(dir.path(), 0o700);
        assert!(keystore_in(&dir).load().unwrap().is_empty());
        assert_eq!(
            coordinator.inner.read().await.keys.len(),
            1,
            "in use whether saved or not"
        );

        coordinator.store_keys(MAC, &KEY, None).await;
        assert_eq!(keystore_in(&dir).load().unwrap()[&MAC].enc_key, KEY);
    }

    /// A save goes through whole, and ahead of the next one, though its
    /// caller is dropped halfway - the read loop, with its link's supervisor
    /// stopped as the pair detached. Regression (rules review of #122): the
    /// abort let go of the keystore's lock while the save went on, and the
    /// next save ran alongside it: the older keys, saved last, lost the newer
    /// one from the disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_outlives_its_caller_and_the_next_one_waits() {
        const OTHER_KEY: [u8; 16] = [8; 16];
        let (coordinator, dir) = coordinator_with(inner_with(vec![]));
        let (reached, at_save) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel::<()>();
        *coordinator.before_save.lock().unwrap() = Some(Box::new(move || {
            let _ = reached.send(());
            // Blocking the save's thread, which nothing awaits on.
            let _ = resumed.recv();
        }));

        let first = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.store_keys(AA, &KEY, None).await }
        });
        at_save.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.store_keys(BB, &OTHER_KEY, None).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alongside = second.is_finished();
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap();
        assert!(!alongside, "saved alongside the first");
        assert_eq!(
            keystore_in(&dir).load().unwrap(),
            HashMap::from([
                (AA, PairKeys::new(KEY, None)),
                (BB, PairKeys::new(OTHER_KEY, None))
            ])
        );
    }

    /// Keys are requested on every connection; one already saved is not
    /// written again.
    #[tokio::test]
    async fn a_saved_key_is_not_written_again() {
        let (coordinator, dir) = coordinator_with(inner_with(vec![]));
        let path = dir.path().join("keys.json");
        coordinator.store_keys(MAC, &KEY, None).await;
        assert!(path.exists());

        std::fs::remove_file(&path).unwrap();
        coordinator.store_keys(MAC, &KEY, None).await;
        assert!(!path.exists(), "the same key again is no reason to write");
    }

    /// A reading outlives hours of quiet, so it is still there long after the
    /// AirPods go silent in their case, and goes after [`BLE_CACHE_TTL`].
    #[test]
    fn prune_drops_only_stale_devices() {
        let (fresh, stale) = (AA, BB);
        let mut inner = inner_with(vec![
            (fresh, Duration::from_secs(10 * 60)),
            (stale, Duration::from_secs(31 * 60)),
        ]);
        assert!(!inner.prune(SystemTime::now()), "half an hour is no age");

        // Ten minutes short of the TTL for the fresh one, eleven past for the stale.
        let later = SystemTime::now() + BLE_CACHE_TTL - Duration::from_secs(20 * 60);
        assert!(inner.prune(later));

        assert!(inner.ble.contains_key(&fresh));
        assert!(!inner.ble.contains_key(&stale));
        assert!(!inner.prune(later), "nothing left to expire");
    }

    /// A reading heard after now - the clock set back since - cannot tell
    /// how old it is, and goes rather than staying on show for as long as
    /// the clock was set back by.
    #[test]
    fn prune_drops_a_reading_heard_after_now() {
        let mut inner = inner_with(vec![(AA, Duration::from_secs(10))]);
        let set_back = SystemTime::now() - Duration::from_secs(60 * 60);
        assert!(inner.prune(set_back));
        assert!(inner.ble.is_empty());
    }

    /// An advertisement's reading of a Pro 3, heard `age` ago.
    fn identified_entry(left: u8, age: Duration) -> Entry {
        Entry {
            state: PodState {
                identity: Some(PRO_3),
                lid_open: Some(true),
                left_in_ear: Some(true),
                connection_state: Some(0x05),
                current_ble_mac: Some(BLE_MAC),
                ..ble_state(left)
            },
            last_seen: SystemTime::now() - age,
        }
    }

    /// `pairs` keyed in the keystore in `dir`.
    fn keyed_in(dir: &tempfile::TempDir, pairs: &[Address]) {
        let keys = pairs
            .iter()
            .map(|&mac| (mac, PairKeys::new(KEY, None)))
            .collect();
        keystore_in(dir).save(&keys).unwrap();
    }

    fn readings_store(dir: &tempfile::TempDir) -> readings::Store {
        readings::Store::with_dir(dir.path().to_path_buf())
    }

    /// The point of #170: a pair put away before a relaunch shows its last
    /// reading after it, with its age, rather than nothing until it is
    /// heard again. What was stale by then stays behind.
    #[tokio::test]
    async fn the_last_readings_outlive_a_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        keyed_in(&dir, &[AA]);
        let first = Coordinator::with_keystore(Some(keystore_in(&dir)));
        let heard = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        first.inner.write().await.ble.insert(
            AA,
            Entry {
                last_seen: heard,
                ..identified_entry(70, Duration::ZERO)
            },
        );
        first.save_readings(Duration::ZERO).await;
        drop(first);

        let second = Coordinator::with_keystore(Some(keystore_in(&dir)));
        let state = second.subscribe().borrow().states[&AA].clone();
        assert_eq!(state.source, DataSource::Ble);
        assert_eq!(state.left_battery, Some(70));
        assert_eq!(state.identity, Some(PRO_3));
        let age = age(state.last_seen.unwrap(), SystemTime::now()).unwrap();
        assert!(
            (2 * 60 * 60..2 * 60 * 60 + 60).contains(&age.as_secs()),
            "heard {age:?} ago"
        );
        assert_eq!(state.left_in_ear, None, "stale by the time it loaded");
        assert_eq!(state.lid_open, None);
        assert_eq!(state.connection_state, None);
        assert_eq!(state.current_ble_mac, None);
        assert!(state.restored, "not heard this run");
    }

    /// Only a pair with keys - the cache holds no other - and a reading the
    /// cache would still hold: heard less than a day ago, and not after now.
    #[test]
    fn loading_keeps_only_fresh_readings_of_pairs_with_keys() {
        let dir = tempfile::tempdir().unwrap();
        keyed_in(&dir, &[AA, BB, CC]);
        let now = SystemTime::now();
        let reading = |heard: SystemTime| Reading {
            left_battery: Some(50),
            right_battery: Some(50),
            case_battery: None,
            left_charging: false,
            right_charging: false,
            case_charging: false,
            identity: PRO_3,
            heard,
        };
        readings_store(&dir)
            .save(&HashMap::from([
                (AA, reading(now - Duration::from_secs(60 * 60))),
                (BB, reading(now - BLE_CACHE_TTL - Duration::from_secs(60))),
                (CC, reading(now + Duration::from_secs(60 * 60))),
                (DD, reading(now)),
            ]))
            .unwrap();

        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));
        let snapshot = coordinator.subscribe().borrow().clone();
        assert_eq!(snapshot.states.keys().collect::<Vec<_>>(), [&AA]);
    }

    /// A readings file that does not parse costs the readings, never the
    /// keys, and is kept to be looked at.
    #[test]
    fn a_readings_file_that_does_not_parse_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        keyed_in(&dir, &[AA]);
        std::fs::write(readings_store(&dir).path(), b"not JSON").unwrap();

        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));
        let snapshot = coordinator.subscribe().borrow().clone();
        assert!(snapshot.states.is_empty());
        assert_eq!(snapshot.known_keys, [AA]);
        assert!(!readings_store(&dir).path().exists());
        let aside = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("readings.json.corrupt-"))
            .count();
        assert_eq!(aside, 1);
    }

    /// Saves are coalesced: nothing is written while nothing worth a write
    /// has changed - a heard time alone moving less than `HEARD_REFRESH` -
    /// and the quit writes the heard times as they are.
    #[tokio::test]
    async fn the_readings_are_saved_only_when_due() {
        let (coordinator, dir) = coordinator_with(inner_with(vec![]));
        let path = readings_store(&dir).path();
        let set = |left: u8, age: Duration| {
            let coordinator = coordinator.clone();
            async move {
                coordinator
                    .inner
                    .write()
                    .await
                    .ble
                    .insert(AA, identified_entry(left, age));
            }
        };

        coordinator.save_readings(readings::HEARD_REFRESH).await;
        assert!(!path.exists(), "nothing to save");

        set(70, Duration::from_secs(120)).await;
        coordinator.save_readings(readings::HEARD_REFRESH).await;
        assert!(path.exists(), "a pair come");

        std::fs::remove_file(&path).unwrap();
        set(70, Duration::from_secs(60)).await;
        coordinator.save_readings(readings::HEARD_REFRESH).await;
        assert!(
            !path.exists(),
            "heard again a minute on: not yet worth a write"
        );

        set(69, Duration::from_secs(60)).await;
        coordinator.save_readings(readings::HEARD_REFRESH).await;
        assert_eq!(
            readings_store(&dir).load().unwrap()[&AA].left_battery,
            Some(69)
        );

        std::fs::remove_file(&path).unwrap();
        set(69, Duration::ZERO).await;
        coordinator.save_readings(Duration::ZERO).await;
        assert!(path.exists(), "the quit's, with the heard time as it is");

        std::fs::remove_file(&path).unwrap();
        coordinator.inner.write().await.ble.clear();
        coordinator.save_readings(readings::HEARD_REFRESH).await;
        assert!(
            readings_store(&dir).load().unwrap().is_empty(),
            "a pair gone"
        );
    }

    /// Undecryptable advertisements are not kept: keyed by a randomized MAC
    /// they can never be attributed to anything, and storing them - an entry
    /// and a broadcast each - is what a BLE spam flood fed on.
    #[tokio::test]
    async fn unidentified_advertisements_are_not_stored() {
        let (coordinator, _dir) = coordinator_with(inner_with(vec![]));
        let mut rx = coordinator.subscribe();
        let _ = rx.borrow_and_update();

        coordinator
            .handle_advertisement(ProximityData::default(), BLE_MAC)
            .await;

        assert!(coordinator.inner.read().await.ble.is_empty());
        assert!(!rx.has_changed().unwrap(), "nothing to broadcast");
    }

    /// A consumer that falls behind holds the latest snapshot, not a queue of
    /// every one it missed.
    #[test]
    fn a_slow_subscriber_holds_one_snapshot() {
        let (coordinator, _dir) = coordinator_with(inner_with(vec![]));
        let mut rx = coordinator.subscribe();
        let nth = |i: u16| {
            let [hi, lo] = i.to_be_bytes();
            Address::new([0, 0, 0, 0, hi, lo])
        };

        let mut inner = inner_with(vec![]);
        for i in 0..1000 {
            inner.keys = HashMap::from([(nth(i), PairKeys::new(KEY, None))]);
            coordinator.broadcast(&inner);
        }

        assert_eq!(rx.borrow_and_update().known_keys, [nth(999)]);
        assert!(!rx.has_changed().unwrap());
    }

    /// Regression (#88): a snapshot went out after the lock it was built under
    /// had been released, so an update in between could send a newer one
    /// first, and the older one landed last - on show until the next
    /// broadcast. Here the first of two battery packets stops right before its
    /// send, and the second gets as far as it can meanwhile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshots_go_out_in_the_order_the_state_changed() {
        let (coordinator, client, _dir) = linked();
        let (paused, on_its_way) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel::<()>();
        *coordinator.before_send.lock().unwrap() = Some(Box::new(move || {
            let _ = paused.send(());
            // Blocking its worker thread, as the send it holds up never awaits.
            let _ = resumed.recv();
        }));

        let first = tokio::spawn({
            let (coordinator, client) = (coordinator.clone(), client.clone());
            async move {
                coordinator
                    .handle_battery_info(battery_packet(83), MAC, &client)
                    .await;
            }
        });
        on_its_way.await.unwrap();

        let second = coordinator.handle_battery_info(battery_packet(82), MAC, &client);
        tokio::pin!(second);
        // All the way, unless the first still holds the lock.
        let done = futures_util::poll!(&mut second).is_ready();
        resume.send(()).unwrap();
        if !done {
            second.await;
        }
        first.await.unwrap();

        assert_eq!(
            coordinator.subscribe().borrow().states[&MAC].left_battery,
            Some(82),
            "the first packet's snapshot landed last"
        );
    }

    /// Regression: the window came up blank with no device connected and none in
    /// range, because nothing was broadcast until the first advertisement arrived.
    #[tokio::test]
    async fn subscribe_delivers_current_state_immediately() {
        let dir = tempfile::tempdir().unwrap();
        keystore_in(&dir)
            .save(&HashMap::from([(MAC, PairKeys::new(KEY, None))]))
            .unwrap();
        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));
        let rx = coordinator.subscribe();

        assert!(
            rx.has_changed().unwrap(),
            "a subscriber must receive the current state without waiting"
        );
        // The snapshot must carry the loaded key list so the UI can render known
        // devices before anything is heard over the air.
        assert_eq!(rx.borrow().known_keys, [MAC]);
    }

    fn aap_state(left: u8) -> PodState {
        PodState {
            left_battery: Some(left),
            ..PodState::empty(DataSource::Aap)
        }
    }

    fn ble_state(left: u8) -> PodState {
        PodState {
            left_battery: Some(left),
            ..PodState::empty(DataSource::Ble)
        }
    }

    /// From its first packet a link shows its exact reading, and the cached
    /// advertisement stays out of sight - AAP is always live. Until then the
    /// advertisement shows. Regression (#62): hidden as the link came up, the
    /// device showed no reading meanwhile, and its GNOME Settings battery
    /// went and came back with every reconnect.
    #[test]
    fn a_link_shows_the_ble_reading_until_its_first_packet() {
        let mut inner = inner_with(vec![]);
        inner.ble.insert(
            AA,
            Entry {
                state: ble_state(70),
                last_seen: SystemTime::now(),
            },
        );
        inner.links.insert(AA, link(None));

        let snap = inner.snapshot();
        assert!(snap.is_connected(AA));
        assert_eq!(snap.states[&AA].source, DataSource::Ble);
        assert_eq!(snap.states[&AA].left_battery, Some(70));

        inner.links.get_mut(&AA).unwrap().reading = Some(aap_state(83));
        let snap = inner.snapshot();
        assert_eq!(snap.states[&AA].source, DataSource::Aap);
        assert_eq!(snap.states[&AA].left_battery, Some(83));
        assert_eq!(snap.states[&AA].last_seen, None);
    }

    /// The point of the cache: after a disconnect the last advertisement shows at
    /// once, instead of nothing until the device next advertises. Regression, too:
    /// a dropped link once left the UI reporting Source: AAP forever.
    #[test]
    fn disconnect_falls_back_to_the_cached_advertisement() {
        let seen = SystemTime::now() - Duration::from_secs(90);
        let mut inner = inner_with(vec![]);
        inner.ble.insert(
            AA,
            Entry {
                state: ble_state(70),
                last_seen: seen,
            },
        );
        inner.links.insert(AA, link(Some(aap_state(83))));

        // What disconnect_aap does.
        inner.links.remove(&AA);

        let snap = inner.snapshot();
        assert_eq!(snap.states[&AA].source, DataSource::Ble);
        assert_eq!(snap.states[&AA].left_battery, Some(70));
        assert_eq!(snap.states[&AA].last_seen, Some(seen));
    }

    /// Two pairs on AAP at once each show their own exact reading, and closing
    /// one link leaves the other alone.
    #[test]
    fn links_are_per_device() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(83))));
        inner.links.insert(BB, link(Some(aap_state(41))));

        let snap = inner.snapshot();
        assert_eq!(snap.states[&AA].left_battery, Some(83));
        assert_eq!(snap.states[&BB].left_battery, Some(41));
        assert!(snap.is_connected(AA) && snap.is_connected(BB));

        inner.links.remove(&AA);
        let snap = inner.snapshot();
        assert!(!snap.is_connected(AA));
        assert!(!snap.states.contains_key(&AA));
        assert_eq!(snap.states[&BB].source, DataSource::Aap);
    }

    /// Connection order, which `Snapshot::default_device` goes by.
    #[test]
    fn connected_macs_follow_connection_order() {
        let now = Instant::now();
        let mut inner = inner_with(vec![]);
        for (mac, age) in [(BB, 5), (CC, 1), (AA, 10)] {
            let mut l = link(None);
            l.since = now - Duration::from_secs(age);
            inner.links.insert(mac, l);
        }
        assert_eq!(inner.snapshot().connected_macs, [AA, BB, CC]);
    }

    /// A read loop outliving its link - the device reconnected while it was
    /// parked in recv - must neither close the new link nor write into it.
    /// Regression: the old loop went on to read the new socket, and stored the
    /// key it found there under the old device's MAC.
    #[tokio::test]
    async fn a_stale_read_loop_cannot_touch_a_newer_link() {
        let stale = link(None).client;
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(None));
        let (coordinator, _dir) = coordinator_with(inner);

        coordinator
            .handle_battery_info(aap::BatteryInfo::default(), AA, &stale)
            .await;
        assert!(coordinator.inner.read().await.links[&AA].reading.is_none());

        assert!(!coordinator.drop_link(AA, Some(&stale)).await);
        assert!(coordinator.inner.read().await.links.contains_key(&AA));

        coordinator.disconnect_aap(AA).await;
        assert!(coordinator.inner.read().await.links.is_empty());
    }

    /// The mode is only reported over AAP, so it may only be shown for the
    /// device that link is up to. Attached in `snapshot` rather than carried
    /// with the battery reading, which every battery packet replaces.
    #[test]
    fn snapshot_attaches_the_mode_to_the_connected_device_only() {
        let mut inner = inner_with(vec![(BB, Duration::from_secs(1))]);
        let mut aa = link(Some(aap_state(80)));
        aa.noise_mode
            .report(aap::NoiseMode::Adaptive, Instant::now());
        inner.links.insert(AA, aa);

        let snap = inner.snapshot();
        assert_eq!(snap.states[&AA].noise_mode, Some(aap::NoiseMode::Adaptive));
        assert_eq!(
            snap.states[&BB].noise_mode, None,
            "a device we cannot command must not show a selected mode"
        );

        // A battery packet replaces the whole state; the mode survives because it
        // never lived there.
        inner.links.get_mut(&AA).unwrap().reading = Some(aap_state(79));
        assert_eq!(
            inner.snapshot().states[&AA].noise_mode,
            Some(aap::NoiseMode::Adaptive)
        );
    }

    /// A device that has not reported a mode yet gets no selection, rather than
    /// the first mode in the list looking active.
    #[test]
    fn snapshot_leaves_an_unreported_mode_unset() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        assert_eq!(inner.snapshot().states[&AA].noise_mode, None);
    }

    /// Regression (#43): the mode outlived its link, so a reconnect showed the
    /// previous session's as selected - though it may have been changed from a
    /// phone meanwhile - until the device next reported one.
    #[tokio::test]
    async fn the_mode_goes_with_the_link() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        coordinator
            .handle_noise_mode(aap::NoiseMode::Adaptive, AA, &client)
            .await;
        assert_eq!(
            coordinator.subscribe().borrow().states[&AA].noise_mode,
            Some(aap::NoiseMode::Adaptive)
        );

        assert!(coordinator.drop_link(AA, Some(&client)).await);
        // The reconnect, as `open_aap` leaves it and the first battery
        // packet after.
        coordinator
            .inner
            .write()
            .await
            .links
            .insert(AA, link(Some(aap_state(80))));
        assert_eq!(
            coordinator.inner.read().await.snapshot().states[&AA].noise_mode,
            None
        );
    }

    /// Conversation Awareness and the speech level are the link's, as the
    /// mode is: shown for the device on AAP once reported, kept across
    /// battery packets, and gone with the link - and a stale read loop's
    /// report lands nowhere.
    #[tokio::test]
    async fn conversation_awareness_and_the_level_go_with_the_link() {
        let mut inner = inner_with(vec![(BB, Duration::from_secs(1))]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        let shown = |coordinator: &Coordinator, mac| {
            let snapshot = coordinator.subscribe().borrow().clone();
            let state = &snapshot.states[&mac];
            (state.conversation_awareness, state.speech_level)
        };
        assert_eq!(shown(&coordinator, AA), (None, None));

        coordinator
            .handle_conversation_awareness(true, AA, &client)
            .await;
        coordinator.handle_speech_level(0x09, AA, &client).await;
        assert_eq!(shown(&coordinator, AA), (Some(true), Some(0x09)));
        assert_eq!(shown(&coordinator, BB), (None, None), "BLE carries neither");
        coordinator
            .handle_battery_info(aap::BatteryInfo::default(), AA, &client)
            .await;
        assert_eq!(shown(&coordinator, AA), (Some(true), Some(0x09)));

        let stale = link(None).client;
        coordinator
            .handle_conversation_awareness(false, AA, &stale)
            .await;
        coordinator.handle_speech_level(0x01, AA, &stale).await;
        assert_eq!(shown(&coordinator, AA), (Some(true), Some(0x09)));

        assert!(coordinator.drop_link(AA, Some(&client)).await);
        coordinator
            .inner
            .write()
            .await
            .links
            .insert(AA, link(Some(aap_state(80))));
        let snapshot = coordinator.inner.read().await.snapshot();
        assert_eq!(snapshot.states[&AA].conversation_awareness, None);
        assert_eq!(snapshot.states[&AA].speech_level, None);
    }

    /// Every level a link brings is told as it comes, a repeated one too,
    /// and the link's going after its last; a stale read loop's level is
    /// told to nobody, and a drop that finds no link tells nothing.
    #[tokio::test]
    async fn speech_events_are_every_level_then_the_link_going() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        let mut events = coordinator.speech_events();
        let gone_consumer = coordinator.speech_events();
        drop(gone_consumer);

        for level in [0x01, 0x02, 0x02, 0x03, 0x08] {
            coordinator.handle_speech_level(level, AA, &client).await;
        }
        coordinator
            .handle_speech_level(0x01, AA, &link(None).client)
            .await;
        assert!(coordinator.drop_link(AA, Some(&client)).await);
        assert!(!coordinator.drop_link(AA, None).await);

        let mut told = Vec::new();
        while let Ok(event) = events.try_recv() {
            told.push(event);
        }
        let level = |level| SpeechEvent::Level { mac: AA, level };
        assert_eq!(
            told,
            [
                level(0x01),
                level(0x02),
                level(0x02),
                level(0x03),
                level(0x08),
                SpeechEvent::LinkGone { mac: AA },
            ]
        );
        assert_eq!(
            coordinator.speech_consumers().len(),
            1,
            "the dropped one goes"
        );
    }

    /// A level, or a setting, the same as the last is not worth waking the
    /// window, tray, attachment and notifier for.
    #[tokio::test]
    async fn an_unchanged_level_or_setting_is_not_broadcast() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        coordinator.handle_speech_level(0x09, AA, &client).await;
        coordinator
            .handle_conversation_awareness(true, AA, &client)
            .await;
        let mut updates = coordinator.subscribe();
        updates.borrow_and_update();

        coordinator.handle_speech_level(0x09, AA, &client).await;
        coordinator
            .handle_conversation_awareness(true, AA, &client)
            .await;
        assert!(!updates.has_changed().unwrap());
        coordinator.handle_speech_level(0x01, AA, &client).await;
        assert!(updates.has_changed().unwrap());
    }

    /// A report of the mode a command just replaced neither puts it back on
    /// show nor is broadcast; the report of the mode set ends that, and a
    /// change made elsewhere is taken again (#113). The command is recorded
    /// where `set_noise_control` records it, since sending needs a socket.
    #[tokio::test]
    async fn a_stale_report_does_not_undo_a_command() {
        use aap::NoiseMode::{NoiseCancellation, Off};

        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        let mode =
            |coordinator: &Coordinator| coordinator.subscribe().borrow().states[&AA].noise_mode;
        coordinator
            .handle_noise_mode(NoiseCancellation, AA, &client)
            .await;
        {
            let mut inner = coordinator.inner.write().await;
            assert!(inner.record_noise_mode(AA, &client, Off));
            coordinator.broadcast(&inner);
        }
        let mut rx = coordinator.subscribe();
        let _ = rx.borrow_and_update();

        coordinator
            .handle_noise_mode(NoiseCancellation, AA, &client)
            .await;
        assert_eq!(mode(&coordinator), Some(Off));
        assert!(!rx.has_changed().unwrap(), "nothing to broadcast");

        coordinator.handle_noise_mode(Off, AA, &client).await;
        coordinator
            .handle_noise_mode(NoiseCancellation, AA, &client)
            .await;
        assert_eq!(mode(&coordinator), Some(NoiseCancellation));
    }

    /// A read loop outliving its link reports the old session's mode, which
    /// must not land on the newer link.
    #[tokio::test]
    async fn a_stale_read_loop_cannot_set_the_mode_on_a_newer_link() {
        let stale = link(None).client;
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let (coordinator, _dir) = coordinator_with(inner);
        let mut rx = coordinator.subscribe();
        let _ = rx.borrow_and_update();

        coordinator
            .handle_noise_mode(aap::NoiseMode::Adaptive, AA, &stale)
            .await;

        assert_eq!(
            coordinator.inner.read().await.links[&AA].noise_mode.mode(),
            None
        );
        assert!(!rx.has_changed().unwrap(), "nothing to broadcast");
    }

    /// A command the link was replaced under - the device reconnected while it
    /// was being sent - records its mode on neither link. Driven where
    /// `set_noise_control` records, since sending needs a real socket.
    #[test]
    fn a_command_spanning_a_reconnect_is_not_recorded_on_the_new_link() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(Some(aap_state(80))));
        let sent_on = inner.links[&AA].client.clone();
        inner.links.insert(AA, link(Some(aap_state(80))));

        assert!(!inner.record_noise_mode(AA, &sent_on, aap::NoiseMode::Off));
        assert_eq!(inner.snapshot().states[&AA].noise_mode, None);

        let current = inner.links[&AA].client.clone();
        assert!(inner.record_noise_mode(AA, &current, aap::NoiseMode::Off));
        assert!(
            !inner.record_noise_mode(AA, &current, aap::NoiseMode::Off),
            "an unchanged mode is not worth a broadcast"
        );
    }

    /// MAC on AAP with KEY stored for it, as after the key exchange; and the
    /// link's client.
    fn linked() -> (Arc<Coordinator>, Arc<aap::Client>, tempfile::TempDir) {
        let mut inner = inner_with(vec![]);
        inner.keys.insert(MAC, PairKeys::new(KEY, None));
        let l = link(None);
        let client = l.client.clone();
        inner.links.insert(MAC, l);
        let (coordinator, dir) = coordinator_with(inner);
        (coordinator, client, dir)
    }

    fn battery_packet(left: u8) -> aap::BatteryInfo {
        aap::BatteryInfo {
            left: Some(aap::battery::Battery {
                component: aap::battery::Component::Left,
                level: Some(left),
                status: aap::battery::Status::Discharging,
            }),
            ..Default::default()
        }
    }

    /// An AirPods Pro 3 advertisement that KEY decrypts, its suffix field zeroed
    /// as pods out of the case send it. Encrypted here, so no real key is needed.
    /// Both pods out of the ears, the left one at 80%.
    fn advertisement() -> ProximityData {
        advertisement_with_left(80)
    }

    /// What [`advertisement`] identifies.
    const PRO_3: Identity = Identity {
        product_id: 0x2027,
        color: 0x01,
    };

    /// [`advertisement`], with the left pod at `left`%.
    fn advertisement_with_left(left: u8) -> ProximityData {
        use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};

        let mut plain = [0u8; 16];
        plain[1..4].copy_from_slice(&[left, 80, 50]); // left, right, case
        let mut block = Block::<aes::Aes128>::try_from(&plain[..]).unwrap();
        aes::Aes128::new_from_slice(&KEY)
            .unwrap()
            .encrypt_block(&mut block);

        // The encrypted portion starts at byte 9 of the payload.
        let mut raw_data = vec![0; 9];
        raw_data.extend_from_slice(&block);
        ProximityData {
            product_id: 0x2027,
            color: 0x01,
            raw_data,
            ..Default::default()
        }
    }

    /// Regression (#44): a battery packet that arrived before any advertisement
    /// left the model blank - "AirPods" in the status line - until the next
    /// packet, which over AAP comes only when a level changes. Advertisements
    /// kept arriving, but none was broadcast for a device on AAP.
    #[tokio::test]
    async fn an_advertisement_names_a_device_already_on_aap() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        let mut rx = coordinator.subscribe();
        assert_eq!(rx.borrow_and_update().states[&MAC].identity, None);

        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(rx.has_changed().unwrap(), "the model is worth a broadcast");
        let state = rx.borrow_and_update().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!(state.left_battery, Some(83));
        assert_eq!(state.identity, Some(PRO_3));
        assert_eq!(PRO_3.model_name(), "AirPods Pro 3");

        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(!rx.has_changed().unwrap(), "nothing new to show");
    }

    /// The identity is the link's: it outlives the advertisement it came from,
    /// whose cache entry expires while the link may well last, and every
    /// battery packet after.
    #[tokio::test]
    async fn the_identity_outlives_the_cached_advertisement() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(
            coordinator
                .inner
                .write()
                .await
                .prune(SystemTime::now() + BLE_CACHE_TTL)
        );

        coordinator
            .handle_battery_info(battery_packet(82), MAC, &client)
            .await;
        let state = coordinator.subscribe().borrow().states[&MAC].clone();
        assert_eq!(state.left_battery, Some(82));
        assert_eq!(state.identity, Some(PRO_3));
    }

    /// Regression (#45): the in-ear icons vanished the moment a pair connected
    /// over AAP, whose battery packets say nothing about ears, although its
    /// advertisements kept saying it. The snapshot takes it from the cached
    /// advertisement until the link has reported it, and then from the link
    /// (#142): a connected pair's payload changes seldom, so the icons lagged
    /// or missed changes. The link's in-ear goes with the link, not with the
    /// advertisement's cache entry; the lid and connection state still come
    /// from that.
    #[tokio::test]
    async fn a_device_on_aap_shows_in_ear_from_its_link_once_reported() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        let left_in = ProximityData {
            left_in_ear: true,
            connection_state: 0x05,
            ..advertisement()
        };
        coordinator.handle_advertisement(left_in, BLE_MAC).await;

        let state = coordinator.subscribe().borrow().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!(state.left_battery, Some(83), "AAP's level, not BLE's");
        assert_eq!(
            (state.left_in_ear, state.right_in_ear),
            (Some(true), Some(false)),
            "the advertisement's, before the link has reported"
        );
        assert_eq!(state.connection_state, Some(0x05));

        // The right pod primary and worn, the left out: not what the
        // advertisement said, nor its primary.
        coordinator
            .handle_primary(Some(PodSide::Right), MAC, &client)
            .await;
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        let state = coordinator.subscribe().borrow().states[&MAC].clone();
        assert_eq!(
            (state.left_in_ear, state.right_in_ear),
            (Some(false), Some(true))
        );
        assert_eq!(state.connection_state, Some(0x05), "the advertisement's");

        assert!(
            coordinator
                .inner
                .write()
                .await
                .prune(SystemTime::now() + BLE_CACHE_TTL)
        );
        let state = coordinator.inner.read().await.snapshot().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!(
            (state.left_in_ear, state.right_in_ear),
            (Some(false), Some(true)),
            "the link's, which outlives the advertisement"
        );
        assert_eq!(state.connection_state, None);
        assert_eq!(state.identity, Some(PRO_3), "the link's, unlike these");
    }

    /// The address each pod of MAC's pair advertises from in
    /// [`left_worn_right_in_case`].
    fn pod_address(side: PodSide) -> Address {
        match side {
            PodSide::Left => BLE_MAC,
            PodSide::Right => Address::new([0x6A, 0x15, 0x2E, 0x93, 0x07, 0xC4]),
        }
    }

    /// What the pod on `side` of MAC's pair advertises with the left pod worn
    /// and the right one in the open case, as in the #71 captures: each pod
    /// calls itself primary, its battery first in the encrypted portion, and
    /// the worn one has no lid to report, only status bit 4. Encrypted with
    /// KEY here, and parsed as the scanner would.
    fn left_worn_right_in_case(side: PodSide) -> ProximityData {
        use crate::ble::decrypt::{Plaintext, ZEROED_SUFFIX, encrypt};
        use crate::ble::parser::{Cleartext, parse_proximity_data, proximity_message};

        let (left, right, case) = ((Some(70), false), (Some(90), true), Some(50));
        let in_case = side == PodSide::Right;
        let cleartext = Cleartext {
            product_id: PRO_3.product_id,
            primary: side,
            this_in_case: in_case,
            both_in_case: false,
            one_in_case: true,
            left_battery: left.0,
            right_battery: right.0,
            case_battery: case,
            left_charging: left.1,
            right_charging: right.1,
            case_charging: false,
            left_in_ear: true,
            right_in_ear: false,
            lid_open: in_case.then_some(true),
            color: PRO_3.color,
            connection_state: 0x04,
        };
        let (this, other) = match side {
            PodSide::Left => (left, right),
            PodSide::Right => (right, left),
        };
        let plaintext = Plaintext {
            header: 0x04,
            primary_battery: this.0,
            primary_charging: this.1,
            secondary_battery: other.0,
            secondary_charging: other.1,
            case_battery: case,
            case_charging: false,
            mac_suffix: ZEROED_SUFFIX,
            tail: [u8::from(in_case); 4],
        };
        let encrypted = encrypt(&plaintext.to_bytes(), &KEY);
        parse_proximity_data(&proximity_message(&cleartext.to_bytes(), &encrypted)).unwrap()
    }

    /// Regression (#152): each pod advertises as itself and calls itself
    /// primary, so a snapshot that took the primary pod from the last
    /// advertisement broadcast for a pair on AAP every time the other pod
    /// was heard - with a pod in the case, at every payload change of either.
    /// The two pods' advertisements of one state show the same, and only the
    /// first is worth a broadcast.
    #[tokio::test]
    async fn the_other_pods_advertisement_of_the_same_state_is_not_broadcast() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        let mut rx = coordinator.subscribe();
        rx.mark_unchanged();

        coordinator
            .handle_advertisement(
                left_worn_right_in_case(PodSide::Right),
                pod_address(PodSide::Right),
            )
            .await;
        assert!(
            rx.has_changed().unwrap(),
            "the model, lid and ears are news"
        );
        rx.mark_unchanged();

        for side in [PodSide::Left, PodSide::Right, PodSide::Left] {
            coordinator
                .handle_advertisement(left_worn_right_in_case(side), pod_address(side))
                .await;
            assert!(
                !rx.has_changed().unwrap(),
                "the {side:?} pod's advertisement shows nothing new"
            );
        }
        let state = rx.borrow().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!(
            (state.left_in_ear, state.right_in_ear),
            (Some(true), Some(false))
        );
        assert_eq!(state.lid_open, Some(true), "the pod in the case's");
        assert_eq!(state.connection_state, Some(0x04));
    }

    /// An ear report: the primary pod's place, then the secondary's.
    fn ear(primary: Place, secondary: Place) -> EarReport {
        EarReport { primary, secondary }
    }

    /// The device's in-ear as the latest snapshot shows it, left then right.
    fn in_ear(coordinator: &Coordinator) -> (Option<bool>, Option<bool>) {
        let snapshot = coordinator.subscribe().borrow().clone();
        let state = &snapshot.states[&MAC];
        (state.left_in_ear, state.right_in_ear)
    }

    /// A linked device with a reading, the left pod primary, both pods worn,
    /// and no advertisement cached.
    async fn worn() -> (Arc<Coordinator>, Arc<aap::Client>, tempfile::TempDir) {
        let (coordinator, client, dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InEar), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)));
        (coordinator, client, dir)
    }

    /// The #142 capture at 17:04:19: the left pod, primary, taken out; the
    /// right one, still worn, primary 0.9 s later, with the ear report in the
    /// new order a few milliseconds before the primary pod packet. Each pod
    /// stays where it is throughout: the report, read with the old primary,
    /// would swap them, and is held for the primary pod - not broadcast.
    #[tokio::test]
    async fn taking_the_primary_pod_out_moves_the_role_and_the_pods_stay_put() {
        let (coordinator, client, _dir) = worn().await;
        coordinator
            .handle_ear_report(ear(Place::Out, Place::InEar), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(false), Some(true)));

        let mut rx = coordinator.subscribe();
        rx.mark_unchanged();
        let held = coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        let until = held.expect("held for the primary pod");
        // Its time given, so that a slow run cannot end the hold here.
        assert_eq!(
            coordinator
                .release_ear(MAC, &client, until - Duration::from_millis(1))
                .await,
            held,
            "held until the hold is over"
        );
        coordinator
            .handle_primary(Some(PodSide::Right), MAC, &client)
            .await;
        assert!(!rx.has_changed().unwrap(), "the pods never shown swapped");
        assert_eq!(in_ear(&coordinator), (Some(false), Some(true)));

        // Back in: both worn, the right pod primary from here on.
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InEar), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)));
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        assert_eq!(
            in_ear(&coordinator),
            (Some(false), Some(true)),
            "the left pod, now secondary, out"
        );
    }

    /// Taking the secondary pod out changes the ear report alone.
    #[tokio::test]
    async fn taking_the_secondary_pod_out_changes_its_pod_alone() {
        let (coordinator, client, _dir) = worn().await;
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(false)));
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InEar), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)));
    }

    /// The UI shows in-ear only: in the case, lid open or closed, is out of
    /// the ear.
    #[tokio::test]
    async fn a_pod_in_the_case_is_out_of_the_ear() {
        let (coordinator, client, _dir) = worn().await;
        for place in [Place::InCase, Place::InClosedCase] {
            coordinator
                .handle_ear_report(ear(Place::InEar, place), MAC, &client)
                .await;
            assert_eq!(in_ear(&coordinator), (Some(true), Some(false)), "{place:?}");
        }
    }

    /// Every link's places are told as they change, from the link's ear state
    /// alone: nothing before it has both an ear report and the primary pod,
    /// nothing for a report that changes no pod's place or is held for a
    /// role switch, and nothing from a stale read loop, an advertisement or
    /// after the link's going.
    #[tokio::test]
    async fn ear_events_are_the_links_places_as_they_change() {
        let (coordinator, client, _dir) = linked();
        let mut events = coordinator.ear_events();
        let gone_consumer = coordinator.ear_events();
        drop(gone_consumer);

        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InEar), MAC, &client)
            .await;
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        // The same again: nothing changed.
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        coordinator
            .handle_ear_report(ear(Place::Out, Place::InEar), MAC, &client)
            .await;
        // The first half of a role switch, held: then read with the new
        // primary, which leaves each pod where it was.
        assert!(
            coordinator
                .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
                .await
                .is_some()
        );
        coordinator
            .handle_primary(Some(PodSide::Right), MAC, &client)
            .await;
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InCase), MAC, &client)
            .await;
        // An advertisement, and a read loop of an older link: neither is
        // the link's ear state.
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        coordinator
            .handle_ear_report(ear(Place::Out, Place::Out), MAC, &link(None).client)
            .await;
        assert!(coordinator.drop_link(MAC, Some(&client)).await);

        let mut told = Vec::new();
        while let Ok(event) = events.try_recv() {
            told.push(event);
        }
        let places = |left, right| EarEvent::Places {
            mac: MAC,
            left,
            right,
        };
        assert_eq!(
            told,
            [
                places(Place::InEar, Place::InEar),
                places(Place::Out, Place::InEar),
                places(Place::InCase, Place::InEar),
                EarEvent::LinkGone { mac: MAC },
            ]
        );
        assert_eq!(coordinator.ear_consumers().len(), 1, "the dropped one goes");
    }

    /// A place the app has no meaning for is not taken for out: the pod keeps
    /// what its advertisement says. A primary pod naming neither pod leaves
    /// the report unreadable, and both pods the advertisement's.
    #[tokio::test]
    async fn an_unknown_value_falls_back_to_the_advertisement() {
        let (coordinator, client, _dir) = worn().await;
        let left_in = ProximityData {
            left_in_ear: true,
            ..advertisement()
        };
        coordinator.handle_advertisement(left_in, BLE_MAC).await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)), "the link's");

        coordinator
            .handle_ear_report(ear(Place::Unknown(0x09), Place::Out), MAC, &client)
            .await;
        assert_eq!(
            in_ear(&coordinator),
            (Some(true), Some(false)),
            "the left pod the advertisement's, the right the link's"
        );
        coordinator
            .handle_ear_report(ear(Place::Out, Place::Unknown(0x09)), MAC, &client)
            .await;
        assert_eq!(
            in_ear(&coordinator),
            (Some(false), Some(false)),
            "the left pod the link's, the right the advertisement's"
        );

        coordinator
            .handle_ear_report(ear(Place::Out, Place::InEar), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(false), Some(true)));
        coordinator.handle_primary(None, MAC, &client).await;
        assert_eq!(
            in_ear(&coordinator),
            (Some(true), Some(false)),
            "the advertisement's, both"
        );
    }

    /// Neither the ear report nor the primary pod is any use alone: until the
    /// link has both, the advertisement shows. After a disconnect it shows
    /// again, at once.
    #[tokio::test]
    async fn the_advertisement_shows_until_the_link_has_reported_and_after_it() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        let left_in = ProximityData {
            left_in_ear: true,
            ..advertisement()
        };
        coordinator.handle_advertisement(left_in, BLE_MAC).await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(false)));

        coordinator
            .handle_ear_report(ear(Place::Out, Place::InEar), MAC, &client)
            .await;
        assert_eq!(
            in_ear(&coordinator),
            (Some(true), Some(false)),
            "no primary pod yet"
        );
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(false), Some(true)));

        coordinator.disconnect_aap(MAC).await;
        let snapshot = coordinator.subscribe().borrow().clone();
        assert_eq!(snapshot.states[&MAC].source, DataSource::Ble);
        assert_eq!(in_ear(&coordinator), (Some(true), Some(false)));
    }

    /// Before the link's first battery packet the advertisement shows, and
    /// nothing the link reports is worth a broadcast; the battery packet
    /// brings what it reported along.
    #[tokio::test]
    async fn reports_before_the_first_battery_packet_come_with_it() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        let mut rx = coordinator.subscribe();
        rx.mark_unchanged();
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        assert!(!rx.has_changed().unwrap(), "nothing new to show");
        assert_eq!(in_ear(&coordinator), (Some(false), Some(false)));

        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(false)));
    }

    /// A report that changes nothing on show is not broadcast: repeated, or
    /// in an advertisement while the link reports in-ear - the pods'
    /// advertisements differed within 0.6 s with both worn (#142).
    #[tokio::test]
    async fn what_changes_nothing_on_show_is_not_broadcast() {
        let (coordinator, client, _dir) = worn().await;
        let mut rx = coordinator.subscribe();
        rx.mark_unchanged();
        coordinator
            .handle_ear_report(ear(Place::InEar, Place::InEar), MAC, &client)
            .await;
        coordinator
            .handle_primary(Some(PodSide::Left), MAC, &client)
            .await;
        assert!(!rx.has_changed().unwrap(), "the same report again");

        // Both pods out, then the left one in, as the advertisements have it
        // while the link reports both worn. The first is news for the
        // connection state it brings, the second for nothing.
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        let left_in = ProximityData {
            left_in_ear: true,
            ..advertisement()
        };
        coordinator.handle_advertisement(left_in, BLE_MAC).await;
        assert!(!rx.has_changed().unwrap(), "in-ear not on show");
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)));

        coordinator
            .handle_ear_report(ear(Place::InEar, Place::Out), MAC, &client)
            .await;
        assert!(rx.has_changed().unwrap(), "a pod taken out");
    }

    /// A report read just before the link was replaced belongs to the old
    /// session, and must not land on the new one.
    #[tokio::test]
    async fn a_report_from_a_replaced_link_is_dropped() {
        let (coordinator, _client, _dir) = worn().await;
        let stale = Arc::new(aap::Client::new(Address::any()));
        coordinator
            .handle_ear_report(ear(Place::Out, Place::Out), MAC, &stale)
            .await;
        coordinator
            .handle_primary(Some(PodSide::Right), MAC, &stale)
            .await;
        assert_eq!(in_ear(&coordinator), (Some(true), Some(true)));
    }

    /// [`advertisement`]'s encrypted portion behind `cleartext`, parsed: an
    /// advertisement KEY decrypts, with the status and lid bytes given.
    fn advertisement_with_cleartext(cleartext: [u8; 9]) -> ProximityData {
        let encrypted = advertisement().encrypted_portion().unwrap().to_vec();
        let data = crate::ble::parser::proximity_message(&cleartext, &encrypted);
        crate::ble::parser::parse_proximity_data(&data).unwrap()
    }

    /// With one pod worn and the other in the open case, both advertise: the
    /// one in the case reads the lid, the worn one none but status bit 4 (the
    /// first #71 capture). The lid stays on show rather than blinking with
    /// every advertisement, until one says no pod is in the case at all.
    #[tokio::test]
    async fn a_worn_pod_keeps_the_lid_its_partner_in_the_case_reported() {
        let mut inner = inner_with(vec![]);
        inner.keys.insert(MAC, PairKeys::new(KEY, None));
        let (coordinator, _dir) = coordinator_with(inner);
        let rx = coordinator.subscribe();
        let lid = || rx.borrow().states[&MAC].lid_open;

        let in_case = [0x01, 0x27, 0x20, 0x73, 0xaa, 0x94, 0x32, 0x00, 0x04];
        let worn = [0x01, 0x27, 0x20, 0x13, 0xaa, 0xa4, 0x11, 0x00, 0x04];
        for _ in 0..3 {
            for (pod, cleartext) in [("in the case", in_case), ("worn", worn)] {
                coordinator
                    .handle_advertisement(advertisement_with_cleartext(cleartext), BLE_MAC)
                    .await;
                assert_eq!(lid(), Some(true), "after the {pod} pod's advertisement");
            }
        }

        let both_worn = [0x01, 0x27, 0x20, 0x0b, 0xaa, 0x8f, 0x11, 0x00, 0x04];
        coordinator
            .handle_advertisement(advertisement_with_cleartext(both_worn), BLE_MAC)
            .await;
        assert_eq!(lid(), None, "no pod is in the case");
    }

    /// A pod taken out of an ear is worth a broadcast for a device on AAP,
    /// though nothing else in the advertisement changed. A level that changed
    /// is not: the snapshot shows AAP's.
    #[tokio::test]
    async fn a_pod_taken_out_is_broadcast_for_a_device_on_aap() {
        let (coordinator, client, _dir) = linked();
        coordinator
            .handle_battery_info(battery_packet(83), MAC, &client)
            .await;
        let both_in = ProximityData {
            left_in_ear: true,
            right_in_ear: true,
            ..advertisement()
        };
        coordinator.handle_advertisement(both_in, BLE_MAC).await;
        let mut rx = coordinator.subscribe();
        assert_eq!(rx.borrow_and_update().states[&MAC].right_in_ear, Some(true));

        let right_out = ProximityData {
            left_in_ear: true,
            ..advertisement()
        };
        coordinator.handle_advertisement(right_out, BLE_MAC).await;
        assert!(
            rx.has_changed().unwrap(),
            "a pod taken out is worth a broadcast"
        );
        assert_eq!(
            rx.borrow_and_update().states[&MAC].right_in_ear,
            Some(false)
        );

        let lower = ProximityData {
            left_in_ear: true,
            ..advertisement_with_left(70)
        };
        coordinator.handle_advertisement(lower, BLE_MAC).await;
        assert!(!rx.has_changed().unwrap(), "nothing new to show");
    }

    /// Regression (#47): a key stored under a lowercase MAC - as
    /// `examples/key_request` stored the MAC it was given - was keyed apart from
    /// the uppercase one BlueZ reports for the same pair. The advertisement it
    /// decrypted and the AAP link were two devices, the link superseded
    /// neither, and the key arriving again over AAP was stored a second time.
    #[tokio::test]
    async fn a_key_stored_under_a_lowercase_mac_belongs_to_the_linked_device() {
        let dir = tempfile::tempdir().unwrap();
        // KEY, in base64.
        let file = r#"{"version":1,"keys":{"aa:bb:cc:dd:ee:ff":"BwcHBwcHBwcHBwcHBwcHBw=="}}"#;
        std::fs::write(dir.path().join("keys.json"), file).unwrap();
        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));

        // The link as `open_aap` opens it, to the address as BlueZ reports it.
        let reported: Address = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let l = link(None);
        let client = l.client.clone();
        coordinator.inner.write().await.links.insert(reported, l);
        coordinator
            .handle_battery_info(battery_packet(83), reported, &client)
            .await;
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;

        let snap = coordinator.subscribe().borrow().clone();
        assert_eq!(snap.devices(), [reported], "one device, not two");
        assert_eq!(snap.states.len(), 1);
        assert_eq!(
            snap.states[&reported].source,
            DataSource::Aap,
            "the link supersedes the advertisement"
        );
        assert!(
            coordinator.inner.read().await.ble.contains_key(&reported),
            "the advertisement was identified as the linked device"
        );

        // Keys are requested on every connection.
        coordinator.store_keys(reported, &KEY, None).await;
        assert_eq!(coordinator.inner.read().await.keys.len(), 1);
        assert_eq!(coordinator.subscribe().borrow().devices(), [reported]);
    }

    /// A coordinator holding KEY for MAC with `irk`, and nothing else.
    fn keyed(irk: Option<[u8; KEY_LEN]>) -> (Arc<Coordinator>, tempfile::TempDir) {
        let mut inner = inner_with(vec![]);
        inner.keys.insert(MAC, PairKeys::new(KEY, irk));
        coordinator_with(inner)
    }

    /// The address a pair holding `irk` might advertise from.
    fn address_of(irk: &[u8; KEY_LEN]) -> Address {
        rpa::resolvable_address(irk, [0x5C, 0x4D, 0x3F])
    }

    /// With an IRK stored, an advertisement counts from an address the IRK
    /// resolves, and from no other, though KEY decrypts it either way (#134).
    #[tokio::test]
    async fn an_advertisement_counts_only_from_an_address_the_irk_resolves() {
        let irk = random_key();
        let (coordinator, _dir) = keyed(Some(irk));
        let elsewhere = address_of(&random_key());
        assert!(rpa::is_resolvable(elsewhere) && rpa::is_resolvable(BLE_MAC));

        for from in [elsewhere, BLE_MAC] {
            coordinator
                .handle_advertisement(advertisement(), from)
                .await;
            assert!(coordinator.inner.read().await.ble.is_empty(), "from {from}");
        }
        let resolving = address_of(&irk);
        coordinator
            .handle_advertisement(advertisement(), resolving)
            .await;
        let state = coordinator.subscribe().borrow().states[&MAC].clone();
        assert_eq!(state.left_battery, Some(80));
        assert_eq!(state.current_ble_mac, Some(resolving));
    }

    /// An address the IRK resolves is not enough: the pair's key has to
    /// decrypt the advertisement too.
    #[tokio::test]
    async fn a_resolving_address_with_another_pairs_payload_is_dropped() {
        let irk = random_key();
        let (coordinator, _dir) = keyed(Some(irk));
        let mut inner = coordinator.inner.write().await;
        inner.keys.get_mut(&MAC).unwrap().enc_key = random_key().to_vec();
        drop(inner);

        coordinator
            .handle_advertisement(advertisement(), address_of(&irk))
            .await;
        assert!(coordinator.inner.read().await.ble.is_empty());
    }

    /// A pair with no IRK yet - keyed before the keystore kept IRKs - is
    /// identified by its key alone, from any address, until its next
    /// connection brings its IRK. A pair with an IRK is not: its key is
    /// tried only on an address its IRK resolves.
    #[tokio::test]
    async fn only_a_pair_without_an_irk_is_identified_by_its_key_alone() {
        let (coordinator, _dir) = keyed(None);
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(coordinator.inner.read().await.ble.contains_key(&MAC));

        let (coordinator, _dir) = keyed(Some(random_key()));
        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(coordinator.inner.read().await.ble.is_empty());
    }

    /// An address is resolved once, not per advertisement; and one that no
    /// IRK resolves is not remembered at all, so a flood of them leaves
    /// nothing behind.
    #[tokio::test]
    async fn only_a_resolved_address_is_remembered() {
        let irk = random_key();
        let (coordinator, _dir) = keyed(Some(irk));
        for _ in 0..3 {
            coordinator
                .handle_advertisement(advertisement(), address_of(&irk))
                .await;
        }
        assert_eq!(coordinator.resolved_addresses().len(), 1);

        for i in 0..100u8 {
            let spammer = rpa::resolvable_address(&random_key(), [i, i, i]);
            coordinator
                .handle_advertisement(advertisement(), spammer)
                .await;
        }
        assert_eq!(coordinator.resolved_addresses().len(), 1);
    }

    /// An address remembered as resolving with an IRK the pair has since
    /// replaced - it was re-paired - is not taken for the pair's.
    #[tokio::test]
    async fn an_address_resolved_with_a_replaced_irk_is_not_the_pairs() {
        let old = random_key();
        let (coordinator, _dir) = keyed(Some(old));
        let address = address_of(&old);
        coordinator
            .handle_advertisement(advertisement(), address)
            .await;
        assert!(coordinator.inner.read().await.ble.contains_key(&MAC));

        let mut inner = coordinator.inner.write().await;
        inner.keys.get_mut(&MAC).unwrap().irk = Some(random_key());
        inner.ble.clear();
        drop(inner);
        coordinator
            .handle_advertisement(advertisement(), address)
            .await;
        assert!(coordinator.inner.read().await.ble.is_empty());
    }

    /// The IRK is stored with the ENC_KEY, and goes to disk with it; a packet
    /// that brings the same keys again writes nothing.
    #[tokio::test]
    async fn an_irk_is_stored_and_saved_with_the_key() {
        let irk = random_key();
        let (coordinator, dir) = coordinator_with(inner_with(vec![]));
        coordinator.store_keys(MAC, &KEY, Some(irk)).await;
        let expected = HashMap::from([(MAC, PairKeys::new(KEY, Some(irk)))]);
        assert_eq!(coordinator.inner.read().await.keys, expected);
        assert_eq!(keystore_in(&dir).load().unwrap(), expected);

        let path = dir.path().join("keys.json");
        std::fs::remove_file(&path).unwrap();
        coordinator.store_keys(MAC, &KEY, Some(irk)).await;
        assert!(!path.exists(), "the same keys again are no reason to write");
    }

    /// A key packet without a usable IRK keeps the one stored, while the
    /// ENC_KEY is the same pairing's. A new ENC_KEY - re-paired - takes the
    /// old IRK with it, as it resolves none of the new pairing's addresses.
    #[tokio::test]
    async fn a_packet_without_an_irk_keeps_the_stored_one_for_the_same_key_only() {
        let irk = random_key();
        let (coordinator, dir) = keyed(Some(irk));

        coordinator.store_keys(MAC, &KEY, None).await;
        assert_eq!(
            coordinator.inner.read().await.keys[&MAC],
            PairKeys::new(KEY, Some(irk))
        );

        let new_key = random_key();
        coordinator.store_keys(MAC, &new_key, None).await;
        let expected = PairKeys::new(new_key, None);
        assert_eq!(coordinator.inner.read().await.keys[&MAC], expected);
        assert_eq!(keystore_in(&dir).load().unwrap()[&MAC], expected);
    }

    /// A command to a device with no link fails, and says so to its caller.
    #[tokio::test]
    async fn a_command_without_a_link_fails() {
        let (coordinator, _dir) = coordinator_with(inner_with(vec![]));
        let off = Command::NoiseControl(aap::NoiseMode::Off);
        let failed = coordinator.command(AA, off).await;
        assert_eq!(
            format!("{:#}", failed.unwrap_err()),
            "switch to Off not sent: no AAP connection to AA:AA:AA:AA:AA:AA"
        );
        let on = Command::ConversationAwareness(true);
        let failed = coordinator.command(AA, on).await;
        assert_eq!(
            format!("{:#}", failed.unwrap_err()),
            "switch of Conversation Awareness on not sent: no AAP connection to AA:AA:AA:AA:AA:AA"
        );
        let rename = Command::Rename("Test Pods".into());
        let failed = coordinator.command(AA, rename).await;
        assert_eq!(
            format!("{:#}", failed.unwrap_err()),
            "rename to \"Test Pods\" not sent: no AAP connection to AA:AA:AA:AA:AA:AA"
        );
    }

    /// A name the pods would not take is refused before anything else is
    /// looked at, link or none.
    #[tokio::test]
    async fn a_name_the_pods_would_not_take_is_refused() {
        let (coordinator, _dir) = coordinator_with(inner_with(vec![]));
        let failed = coordinator
            .command(AA, Command::Rename(String::new()))
            .await;
        assert_eq!(
            format!("{:#}", failed.unwrap_err()),
            "rename to \"\" not sent: the name is empty"
        );
    }

    /// The name is the link's: reported by its startup dump, it shows in the
    /// snapshot from the link's first reading, goes with the link, and one
    /// reported on a replaced link is dropped.
    #[tokio::test]
    async fn the_name_goes_with_the_link() {
        let mut inner = inner_with(vec![]);
        inner
            .links
            .insert(AA, link(Some(PodState::empty(DataSource::Aap))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        let name =
            |coordinator: &Coordinator| coordinator.subscribe().borrow().states[&AA].name.clone();
        assert_eq!(name(&coordinator), None);

        coordinator
            .handle_device_info(info(Ok("Test Pods"), None), AA, &client)
            .await;
        assert_eq!(name(&coordinator).as_deref(), Some("Test Pods"));

        let stale = Arc::new(aap::Client::new(Address::any()));
        coordinator
            .handle_device_info(info(Ok("Old Pods"), None), AA, &stale)
            .await;
        assert_eq!(name(&coordinator).as_deref(), Some("Test Pods"));

        coordinator.disconnect_aap(AA).await;
        let snapshot = coordinator.subscribe().borrow().clone();
        assert!(
            !snapshot.states.contains_key(&AA),
            "no reading but the link's"
        );
    }

    /// Device information as `aap::info` reads it: `name`, and field 10's
    /// value where there is one.
    fn info(name: Result<&str, FieldError>, version: Option<u32>) -> DeviceInfo {
        DeviceInfo {
            name: name.map(str::to_string),
            version: version
                .map(FirmwareVersion::from_raw)
                .ok_or(FieldError::Missing),
        }
    }

    /// The firmware version is the pair's, not the link's: reported by the
    /// startup dump, it stays on show after the link goes, without a
    /// reading, until the next link reports another. One reported on a
    /// replaced link is dropped.
    #[tokio::test]
    async fn the_firmware_version_outlives_the_link() {
        let mut inner = inner_with(vec![]);
        inner.links.insert(AA, link(None));
        let (coordinator, _dir) = coordinator_with(inner);
        let version = |coordinator: &Coordinator| {
            let snapshot = coordinator.subscribe().borrow().clone();
            snapshot.firmware_version(AA).map(|v| v.to_string())
        };
        assert_eq!(version(&coordinator), None);

        let client = client_of(&coordinator, AA).await;
        coordinator
            .handle_device_info(info(Ok("Pods"), Some(9_442_752)), AA, &client)
            .await;
        assert_eq!(version(&coordinator).as_deref(), Some("9A348"));

        let stale = Arc::new(aap::Client::new(Address::any()));
        coordinator
            .handle_device_info(info(Ok("Pods"), Some(0x0081_00E0)), AA, &stale)
            .await;
        assert_eq!(version(&coordinator).as_deref(), Some("9A348"));

        coordinator.disconnect_aap(AA).await;
        let snapshot = coordinator.subscribe().borrow().clone();
        assert!(!snapshot.states.contains_key(&AA));
        assert_eq!(version(&coordinator).as_deref(), Some("9A348"));

        // The next link, after an update.
        coordinator.inner.write().await.links.insert(AA, link(None));
        let client = client_of(&coordinator, AA).await;
        coordinator
            .handle_device_info(info(Ok("Pods"), Some(0x00A0_0010)), AA, &client)
            .await;
        assert_eq!(version(&coordinator).as_deref(), Some("10A1"));
    }

    /// A field that did not read leaves the other: a name that is not
    /// UTF-8 still brings the version, and a version that does not read
    /// leaves the name and the version last reported.
    #[tokio::test]
    async fn each_field_of_the_device_information_is_taken_on_its_own() {
        let mut inner = inner_with(vec![]);
        inner
            .links
            .insert(AA, link(Some(PodState::empty(DataSource::Aap))));
        let (coordinator, _dir) = coordinator_with(inner);
        let client = client_of(&coordinator, AA).await;
        let shown = |coordinator: &Coordinator| {
            let snapshot = coordinator.subscribe().borrow().clone();
            let version = snapshot.firmware_version(AA).map(|v| v.to_string());
            (snapshot.states[&AA].name.clone(), version)
        };

        let unnamed = info(Err(FieldError::NotUtf8), Some(9_442_752));
        coordinator.handle_device_info(unnamed, AA, &client).await;
        assert_eq!(shown(&coordinator), (None, Some("9A348".into())));

        let unversioned = info(Ok("Pods"), None);
        coordinator
            .handle_device_info(unversioned, AA, &client)
            .await;
        assert_eq!(
            shown(&coordinator),
            (Some("Pods".into()), Some("9A348".into()))
        );
    }

    #[test]
    fn aap_supersedes_ble_only_for_the_connected_device() {
        let mut inner = inner_with(vec![]);
        // With nothing connected, everything is processed.
        assert!(!inner.supersedes_ble(AA));

        // Nor while the link has not reported yet.
        inner.links.insert(AA, link(None));
        assert!(!inner.supersedes_ble(AA));

        inner.links.insert(AA, link(Some(aap_state(80))));
        // The connected device's own advertisements are superseded...
        assert!(inner.supersedes_ble(AA));
        // ...but a second pair of AirPods keeps being tracked over BLE.
        assert!(!inner.supersedes_ble(BB));
        // A random BLE MAC is never mistaken for the connected device's real one.
        assert!(!inner.supersedes_ble(BLE_MAC));
    }

    #[test]
    fn lowest_earbud_handles_missing_values() {
        let mut s = PodState {
            left_battery: Some(80),
            right_battery: Some(60),
            ..PodState::empty(DataSource::Ble)
        };
        assert_eq!(s.lowest_earbud(), Some(60));

        s.right_battery = None;
        assert_eq!(s.lowest_earbud(), Some(80));

        s.left_battery = None;
        assert_eq!(s.lowest_earbud(), None);
    }

    /// Regression (#108): a part reported as Disconnected comes with a
    /// placeholder level, `0x00` included. Read as a level, a pod reported that
    /// way would have been the lowest earbud at 0%, and so what GNOME Settings
    /// showed. It is unknown, and the other pod is the lowest known.
    #[tokio::test]
    async fn a_disconnected_earbud_is_not_the_lowest() {
        use aap::battery::{Battery, Component, Status};

        let (coordinator, client, _dir) = linked();
        let battery = |component, level, status| Battery {
            component,
            level: Some(level),
            status,
        };
        let packet = aap::battery::battery_packet(&[
            battery(Component::Right, 0, Status::Disconnected),
            battery(Component::Left, 70, Status::Discharging),
            battery(Component::Case, 0, Status::Disconnected),
        ]);
        let info = aap::parse_battery_packet(&packet).unwrap();
        coordinator.handle_battery_info(info, MAC, &client).await;

        let state = coordinator.subscribe().borrow().states[&MAC].clone();
        assert_eq!(state.right_battery, None);
        assert_eq!(state.case_battery, None);
        assert_eq!(state.lowest_earbud(), Some(70));
    }

    /// The longest link wins, even before it has reported and with another pair
    /// in range: a second pair connecting must not take over.
    #[test]
    fn default_device_is_the_longest_link() {
        let snap = listing(&[AA], &[CC, BB], &[AA, BB]);
        assert_eq!(snap.default_device(), Some(CC));
    }

    /// With nothing on AAP, the first device with a reading - in MAC order, not
    /// whichever the `HashMap` yields first, which changes from one snapshot to
    /// the next and made the tray jump between two pairs.
    #[test]
    fn default_device_falls_back_to_the_first_with_a_reading() {
        let snap = listing(&[AA, BB, CC], &[], &[CC, BB]);
        assert_eq!(snap.default_device(), Some(BB));
    }

    /// A pair we hold a key for is still a device to show, out of range or not.
    #[test]
    fn default_device_falls_back_to_the_first_device() {
        let snap = listing(&[AA, BB], &[], &[]);
        assert_eq!(snap.default_device(), Some(AA));
        assert_eq!(Snapshot::default().default_device(), None);
    }

    /// Keyed and connected devices alike, each once, in MAC order: a pair on AAP
    /// whose key has not arrived is listed, and so is one we hold a key for but
    /// have not heard from.
    #[test]
    fn devices_are_the_known_and_the_connected() {
        let snap = listing(&[BB, DD], &[CC, BB], &[BB]);
        assert_eq!(snap.devices(), [BB, CC, DD]);
        assert!(Snapshot::default().devices().is_empty());
    }

    /// A snapshot holding keys for `known`, links to `connected` in that order,
    /// and a reading for each of `heard`.
    fn listing(known: &[Address], connected: &[Address], heard: &[Address]) -> Snapshot {
        Snapshot {
            states: heard
                .iter()
                .map(|&m| (m, PodState::empty(DataSource::Ble)))
                .collect(),
            connected_macs: connected.to_vec(),
            known_keys: known.to_vec(),
            ..Snapshot::default()
        }
    }

    /// Links over a Unix socket, as the harness opens them.
    #[cfg(feature = "harness")]
    mod unix_socket {
        use tokio::time::timeout;
        use tokio_seqpacket::{UnixSeqpacket, UnixSeqpacketListener};

        use super::*;

        /// Plenty for any of these, and a hang fails the test instead of CI.
        const TIMEOUT: Duration = Duration::from_secs(5);

        /// What `connect_aap` sends, in order: handshake, battery notifications,
        /// special features, proximity keys. Spelled out rather than taken from
        /// the client, so a change on the wire shows here.
        const SEQUENCE: [&[u8]; 4] = [
            &[
                0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ],
            &[0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF],
            &[
                0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            &[0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00],
        ];

        /// How Pro 3 starts its answer to the handshake.
        const HANDSHAKE_REPLY: [u8; 8] = [0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00];

        /// A battery packet: left at 83%, discharging.
        const BATTERY: [u8; 12] = [
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x04, 0x01, 0x53, 0x02, 0x01,
        ];

        /// A coordinator opening links to the sockets in a temporary directory,
        /// its keys kept there too, and a fake listening there for MAC.
        fn harnessed() -> (Arc<Coordinator>, UnixSeqpacketListener, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let coordinator =
                Coordinator::with_aap_socket_dir(Some(keystore_in(&dir)), dir.path().to_path_buf());
            let path = aap::client::unix_socket_path(dir.path(), MAC);
            let listener = UnixSeqpacketListener::bind(path).unwrap();
            (coordinator, listener, dir)
        }

        /// The next message the fake receives, whole.
        async fn recv(fake: &UnixSeqpacket) -> Vec<u8> {
            let mut buf = [0u8; 1024];
            let info = timeout(TIMEOUT, fake.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            buf[..info.bytes_read()].to_vec()
        }

        /// A link to a fake runs as one to the device: the handshake sequence
        /// goes out, the rest of it only once the handshake is answered, the
        /// read loop takes what comes back, and the fake hanging up takes the
        /// link down.
        #[tokio::test]
        async fn connect_aap_handshakes_and_starts_the_read_loop() {
            let (coordinator, mut listener, _dir) = harnessed();
            let connecting = tokio::spawn({
                let coordinator = coordinator.clone();
                async move { coordinator.connect_aap(MAC).await }
            });
            let fake = timeout(TIMEOUT, listener.accept()).await.unwrap().unwrap();
            assert_eq!(recv(&fake).await, SEQUENCE[0], "the handshake");

            // Long enough to catch requests sent after a fixed pause rather than
            // on the answer (`docs/app/aap-links.md#the-handshake`).
            let early = timeout(Duration::from_secs(1), recv(&fake)).await;
            assert!(early.is_err(), "sent ahead of the answer: {early:?}");

            fake.send(&HANDSHAKE_REPLY).await.unwrap();
            for (i, packet) in SEQUENCE.into_iter().enumerate().skip(1) {
                assert_eq!(recv(&fake).await, packet, "packet {i} of the sequence");
            }
            timeout(TIMEOUT, connecting)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            let mut rx = coordinator.subscribe();
            assert!(rx.borrow_and_update().is_connected(MAC));
            fake.send(&BATTERY).await.unwrap();
            let state = timeout(TIMEOUT, rx.wait_for(|s| s.states.contains_key(&MAC)))
                .await
                .unwrap()
                .unwrap()
                .states[&MAC]
                .clone();
            assert_eq!(state.source, DataSource::Aap);
            assert_eq!(state.left_battery, Some(83));

            drop(fake);
            timeout(TIMEOUT, rx.wait_for(|s| !s.is_connected(MAC)))
                .await
                .unwrap()
                .unwrap();
            assert!(coordinator.inner.read().await.links.is_empty());
        }

        /// A device that takes the handshake and hangs up before answering
        /// it leaves no link behind, so its BLE reading stays on show.
        #[tokio::test]
        async fn connect_aap_fails_when_the_device_hangs_up_before_answering() {
            let (coordinator, mut listener, _dir) = harnessed();
            let (connected, ()) = tokio::join!(coordinator.connect_aap(MAC), async {
                let fake = listener.accept().await.unwrap();
                assert_eq!(recv(&fake).await, SEQUENCE[0]);
            });
            assert!(connected.is_err());
            assert!(coordinator.subscribe().borrow().connected_macs.is_empty());
        }

        /// Each pair has a socket of its own. BB has none, though MAC does, and
        /// fails to connect rather than fall back to L2CAP and real AirPods.
        #[tokio::test]
        async fn connect_aap_without_a_listener_fails() {
            let (coordinator, _listener, _dir) = harnessed();
            assert!(coordinator.connect_aap(BB).await.is_err());
            assert!(coordinator.subscribe().borrow().connected_macs.is_empty());
        }

        /// MAC's link to a fake, opened as `open_aap` opens it and not yet
        /// run, and the fake's end of it.
        async fn opened() -> (Arc<Coordinator>, AapLink, UnixSeqpacket, tempfile::TempDir) {
            let (coordinator, mut listener, dir) = harnessed();
            let (link, fake) = open(&coordinator, &mut listener).await;
            (coordinator, link, fake, dir)
        }

        /// MAC's link opened by `coordinator`, the fake answering on
        /// `listener`, and the fake's end of it.
        async fn open(
            coordinator: &Arc<Coordinator>,
            listener: &mut UnixSeqpacketListener,
        ) -> (AapLink, UnixSeqpacket) {
            let opening = tokio::spawn({
                let coordinator = coordinator.clone();
                async move { coordinator.open_aap(MAC).await }
            });
            let fake = timeout(TIMEOUT, listener.accept()).await.unwrap().unwrap();
            assert_eq!(recv(&fake).await, SEQUENCE[0], "the handshake");
            fake.send(&HANDSHAKE_REPLY).await.unwrap();
            for packet in &SEQUENCE[1..] {
                assert_eq!(recv(&fake).await, *packet);
            }
            let link = timeout(TIMEOUT, opening).await.unwrap().unwrap().unwrap();
            (link, fake)
        }

        /// A link's queue goes only with its own link. Here the old link's
        /// record is dropped late, as its run or its drop does once a newer
        /// link is up, and commands still reach the newer one - as a read
        /// loop outliving its link cannot touch a newer link's record.
        #[tokio::test]
        async fn a_stale_link_cannot_take_a_newer_links_queue() {
            let (coordinator, mut listener, _dir) = harnessed();
            let (old, _old_fake) = open(&coordinator, &mut listener).await;
            let old_client = old.client.clone();
            coordinator.disconnect_aap(MAC).await;
            let (new, new_fake) = open(&coordinator, &mut listener).await;

            assert!(!coordinator.drop_link(MAC, Some(&old_client)).await);
            let run = tokio::spawn(new.run());
            let transparency = Command::NoiseControl(aap::NoiseMode::Transparency);
            timeout(TIMEOUT, coordinator.command(MAC, transparency))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                recv(&new_fake).await,
                aap::noise::set_packet(aap::NoiseMode::Transparency)
            );
            drop((old, new_fake));
            timeout(TIMEOUT, run).await.unwrap().unwrap();
        }

        /// A command sent while a dropped link's record is still on its way
        /// out - the drop has to leave that to a task - fails at once.
        #[tokio::test]
        async fn a_command_to_a_link_dropped_but_not_yet_removed_fails() {
            let (coordinator, link, _fake, _dir) = opened().await;
            drop(link);
            // On this one thread, the removal the drop spawned has not run.
            assert!(coordinator.inner.read().await.links.contains_key(&MAC));
            let off = Command::NoiseControl(aap::NoiseMode::Off);
            let failed = coordinator.command(MAC, off).await.unwrap_err();
            assert_eq!(
                format!("{failed:#}"),
                "switch to Off not sent: AAP to AA:BB:CC:DD:EE:FF is closing"
            );
        }

        /// A command its link went in the middle of tells its caller the
        /// connection was lost, as it may have reached the device; not that
        /// it failed.
        #[test]
        fn a_command_cut_short_says_the_connection_was_lost() {
            let (done, result) = oneshot::channel();
            let command = Command::NoiseControl(aap::NoiseMode::Off);
            drop(Underway {
                mac: MAC,
                command,
                done: Some(done),
            });
            let lost = result.blocking_recv().unwrap().unwrap_err();
            assert!(lost.downcast_ref::<ConnectionLost>().is_some(), "{lost:#}");
            assert_eq!(
                lost.to_string(),
                "lost the connection to AA:BB:CC:DD:EE:FF during the switch to Off"
            );
        }

        /// A command queued on a link that goes before carrying it out is
        /// not lost without a word: its caller hears that it was not done,
        /// and nothing goes out (#54).
        #[tokio::test]
        async fn a_command_its_link_drops_unsent_fails() {
            let (coordinator, link, fake, _dir) = opened().await;
            let sent = coordinator.command(MAC, Command::NoiseControl(aap::NoiseMode::Off));
            drop(link);

            let failed = timeout(TIMEOUT, sent).await.unwrap();
            assert_eq!(
                failed.unwrap_err().to_string(),
                "AAP to AA:BB:CC:DD:EE:FF went before the switch to Off was sent"
            );
            let mut buf = [0u8; 64];
            let read = timeout(TIMEOUT, fake.recv(&mut buf)).await.unwrap();
            assert_eq!(read.unwrap().bytes_read(), 0, "closed, with nothing sent");
        }

        /// A send that fails reaches its caller, records no mode, and leaves
        /// the link up: what the device sends is still read.
        #[tokio::test]
        async fn a_command_that_cannot_be_sent_fails() {
            let (coordinator, link, fake, _dir) = opened().await;
            // The app's sends fail from here; the fake's still arrive.
            fake.shutdown(std::net::Shutdown::Read).unwrap();
            let run = tokio::spawn(link.run());

            let transparency = Command::NoiseControl(aap::NoiseMode::Transparency);
            let failed = timeout(TIMEOUT, coordinator.command(MAC, transparency))
                .await
                .unwrap();
            let failed = format!("{:#}", failed.unwrap_err());
            assert!(
                failed.starts_with("failed to send noise control"),
                "{failed}"
            );

            fake.send(&BATTERY).await.unwrap();
            let mut rx = coordinator.subscribe();
            let state = timeout(TIMEOUT, rx.wait_for(|s| s.states.contains_key(&MAC)))
                .await
                .unwrap()
                .unwrap()
                .states[&MAC]
                .clone();
            assert_eq!(state.noise_mode, None, "the mode of a command not sent");
            drop(fake);
            timeout(TIMEOUT, run).await.unwrap().unwrap();
        }

        /// Conversation Awareness goes out as the 0x28 setting, and is
        /// recorded once it is away: nothing the pods send confirms it.
        #[tokio::test]
        async fn conversation_awareness_goes_out_and_is_recorded() {
            let (coordinator, link, fake, _dir) = opened().await;
            let run = tokio::spawn(link.run());
            fake.send(&BATTERY).await.unwrap();
            let mut rx = coordinator.subscribe();
            timeout(TIMEOUT, rx.wait_for(|s| s.states.contains_key(&MAC)))
                .await
                .unwrap()
                .unwrap();

            for enabled in [true, false] {
                let command = Command::ConversationAwareness(enabled);
                timeout(TIMEOUT, coordinator.command(MAC, command))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(recv(&fake).await, aap::conversation::set_packet(enabled));
                let shown = rx.borrow_and_update().states[&MAC].conversation_awareness;
                assert_eq!(shown, Some(enabled));
            }
            drop(fake);
            timeout(TIMEOUT, run).await.unwrap().unwrap();
        }

        /// The name comes from the device information packet, and a rename
        /// goes out as 0x1A and is recorded once it is away: the pods answer
        /// nothing. A name the pods would not take never reaches the queue:
        /// the first packet the fake reads is the rename that was allowed.
        #[tokio::test]
        async fn a_rename_goes_out_and_is_recorded() {
            let (coordinator, link, fake, _dir) = opened().await;
            let run = tokio::spawn(link.run());
            fake.send(&BATTERY).await.unwrap();
            let info = aap::info::info_packet_with_version("Old Pods", "9442752");
            fake.send(&info).await.unwrap();
            let mut rx = coordinator.subscribe();
            let named = |s: &Snapshot| s.states.get(&MAC).and_then(|s| s.name.clone());
            timeout(
                TIMEOUT,
                rx.wait_for(|s| named(s).as_deref() == Some("Old Pods")),
            )
            .await
            .unwrap()
            .unwrap();

            for refused in [String::new(), "a".repeat(128), "a\0b".into()] {
                let command = Command::Rename(refused);
                let failed = coordinator.command(MAC, command).await.unwrap_err();
                assert!(
                    format!("{failed:#}").contains("not sent: the name"),
                    "{failed:#}"
                );
            }
            assert_eq!(named(&rx.borrow_and_update()).as_deref(), Some("Old Pods"));

            let longest = "a".repeat(127);
            let command = Command::Rename(longest.clone());
            timeout(TIMEOUT, coordinator.command(MAC, command))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                recv(&fake).await,
                aap::name::rename_packet(&longest).unwrap()
            );
            assert_eq!(named(&rx.borrow_and_update()), Some(longest));
            drop(fake);
            timeout(TIMEOUT, run).await.unwrap().unwrap();
        }
    }
}
