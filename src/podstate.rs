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
//! state is only ever the live link's and goes with it. While the link is up,
//! the device's snapshot takes from its cached advertisement what AAP does not
//! carry: in-ear, lid, connection state and primary pod.
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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};
use tracing::Instrument;

use crate::KEY_LEN;
use crate::aap::noise::{ModeState, Report};
use crate::aap::{self, Message};
use crate::ble::decode_model_name;
use crate::ble::decrypt::decrypt_for_device;
use crate::ble::parser::{PodSide, ProximityData};
use crate::ble::rpa::{self, ResolvedAddresses};
use crate::keystore::{Keystore, PairKeys};

/// How long the last advertisement of an identified device stays on show. Keyed
/// by the real MAC, these do not rotate, so the map stays bounded by the number of
/// stored keys.
pub const BLE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Switches the noise control mode, and records it.
    NoiseControl(aap::NoiseMode),
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoiseControl(mode) => write!(f, "switch to {mode}"),
        }
    }
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
            command: self.command,
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

    /// Whether each pod is in an ear, from an advertisement like the
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
    /// Only advertisements carry it, along with in-ear, lid and the primary pod.
    /// Unlike the identity these change as the earbuds are used, so for a
    /// device on AAP they are taken from its cached advertisement each time a
    /// snapshot is built: a connected pair keeps advertising, which keeps them
    /// current, and they expire with that cache entry. `None` while no
    /// advertisement is cached.
    pub connection_state: Option<u8>,

    /// From an advertisement like the
    /// [connection state](PodState::connection_state).
    pub primary_pod: Option<PodSide>,

    /// Active noise control mode. `None` until the connected device reports one:
    /// BLE advertisements do not carry it, and a default would show the wrong
    /// mode as selected in the interface.
    pub noise_mode: Option<aap::NoiseMode>,

    /// `None` until an advertisement has identified the device. For one on
    /// AAP it is the link's, which outlives the cached advertisement.
    pub identity: Option<Identity>,

    /// The randomized address the advertisement behind a BLE reading came
    /// from. `None` for AAP, which knows only the real one.
    pub current_ble_mac: Option<Address>,

    /// When the advertisement behind a BLE reading arrived; it may be a cached
    /// one up to [`BLE_CACHE_TTL`] old. `None` for AAP, which is always live.
    pub last_seen: Option<Instant>,
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
            primary_pod: None,
            noise_mode: None,
            identity: None,
            current_ble_mac: None,
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
    /// BlueZ aliases by MAC - the names the rest of the desktop shows for these
    /// devices. Empty until attachment reports them, and it stays empty when
    /// BlueZ is unreachable, so consumers need a fallback.
    pub device_names: HashMap<Address, String>,
}

impl Snapshot {
    /// The BlueZ alias for `mac`, when one is known.
    pub fn device_name(&self, mac: Address) -> Option<&str> {
        self.device_names.get(&mac).map(String::as_str)
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
    last_seen: Instant,
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
    /// Which device the link is to, as last advertised; AAP never says.
    /// Kept apart from the reading like the mode. Taken from the cached
    /// advertisement when the link comes up and refreshed by every one after,
    /// so it outlives that cache entry's expiry.
    identity: Option<Identity>,
    /// Orders `Snapshot::connected_macs`.
    since: Instant,
}

/// The parts of an advertisement that change as the earbuds are used - a pod
/// taken out, the lid opened, a call coming in. AAP carries none of them, so a
/// linked device's snapshot takes them from its cached advertisement.
#[derive(Clone, Copy, PartialEq)]
struct Live {
    left_in_ear: Option<bool>,
    right_in_ear: Option<bool>,
    lid_open: Option<bool>,
    connection_state: Option<u8>,
    primary_pod: Option<PodSide>,
}

impl Live {
    fn of(state: &PodState) -> Self {
        Self {
            left_in_ear: state.left_in_ear,
            right_in_ear: state.right_in_ear,
            lid_open: state.lid_open,
            connection_state: state.connection_state,
            primary_pod: state.primary_pod,
        }
    }

    fn apply_to(self, state: &mut PodState) {
        state.left_in_ear = self.left_in_ear;
        state.right_in_ear = self.right_in_ear;
        state.lid_open = self.lid_open;
        state.connection_state = self.connection_state;
        state.primary_pod = self.primary_pod;
    }
}

struct Inner {
    /// The last advertisement per device - BLE only. The connected device's entry
    /// keeps updating underneath AAP, so it is current when the link drops.
    ble: HashMap<Address, Entry>,
    /// One per live AAP link, keyed by real MAC.
    links: HashMap<Address, Link>,
    /// Each pair's ENC_KEY, and its IRK once one has arrived.
    keys: HashMap<Address, PairKeys>,
    device_names: HashMap<Address, String>,
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
    fn prune(&mut self, now: Instant) -> bool {
        let before = self.ble.len();
        self.ble
            .retain(|_, e| now.duration_since(e.last_seen) < BLE_CACHE_TTL);
        self.ble.len() != before
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
            let Some(reading) = &link.reading else {
                continue;
            };
            let mut state = reading.clone();
            state.noise_mode = link.noise_mode.mode();
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
            states.insert(mac, state);
        }

        let mut links: Vec<(&Address, &Link)> = self.links.iter().collect();
        links.sort_by_key(|(_, link)| link.since);

        Snapshot {
            states,
            connected_macs: links.into_iter().map(|(&mac, _)| mac).collect(),
            known_keys,
            device_names: self.device_names.clone(),
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
    /// Held across a save, so saves never overlap - they share a temporary
    /// file. Owned by the save while it runs - see
    /// `Coordinator::store_keys`.
    keystore: Arc<Mutex<Persistence>>,
    /// The latest snapshot, which every subscriber sees - see [`Coordinator::subscribe`].
    updates: watch::Sender<Snapshot>,
    /// Where [`Coordinator::open_aap`] opens links in place of L2CAP, under
    /// the harness. Fixed at construction, so every link of a session goes the
    /// same way.
    #[cfg(feature = "harness")]
    aap_socket_dir: Option<PathBuf>,
    /// Runs once, in the next broadcast just before it sends, for a test to
    /// hold an update up there.
    #[cfg(test)]
    before_send: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, in the next save just before it writes, for a test to hold
    /// a save up there.
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
        tracing::info!(
            "Harness: AAP links go to {}/<MAC>.sock instead of L2CAP",
            dir.display()
        );
        Arc::new(Self {
            aap_socket_dir: Some(dir),
            ..Self::from_keystore(keystore)
        })
    }

    /// Loads the keystore's keys into a coordinator that opens links over L2CAP.
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

        let persistence = Persistence {
            keystore,
            saved: loaded.clone(),
        };
        let inner = Inner {
            ble: HashMap::new(),
            links: HashMap::new(),
            keys: loaded,
            device_names: HashMap::new(),
        };
        let (updates, _) = watch::channel(inner.snapshot());

        Self {
            inner: RwLock::new(inner),
            queues: std::sync::Mutex::default(),
            resolved: std::sync::Mutex::default(),
            keystore: Arc::new(Mutex::new(persistence)),
            updates,
            #[cfg(feature = "harness")]
            aap_socket_dir: None,
            #[cfg(test)]
            before_send: std::sync::Mutex::default(),
            #[cfg(test)]
            before_save: std::sync::Mutex::default(),
        }
    }

    /// Registers a new consumer, which sees the latest snapshot.
    ///
    /// A watch channel holds one value that every receiver sees, so the UI, tray
    /// and attachment never compete for a snapshot, and one that falls
    /// behind skips stale snapshots rather than queueing them - an unbounded queue
    /// per consumer grew without limit under a flood of advertisements. Sending
    /// never waits for a consumer to take the snapshot, so a slow one cannot
    /// stall a protocol read loop either - only an open `borrow()` holds a send
    /// up; see below.
    ///
    /// Marked changed, so the current state arrives at once. Without that a
    /// subscriber which starts before the first advertisement sees nothing at all -
    /// the window came up blank whenever no device was connected or in range, even
    /// though the keys were already loaded from disk.
    ///
    /// Never hold the receiver's `borrow()` across an await: it is a read lock,
    /// and every broadcast waits for it - holding the coordinator's lock, so
    /// every other update waits too.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        let mut rx = self.updates.subscribe();
        rx.mark_changed();
        rx
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
    /// actually differ - attachment re-reads them on every connection event and
    /// adapter change, and rebroadcasting an identical map would wake the UI,
    /// tray and attachment for nothing.
    pub(crate) async fn set_device_names(&self, names: HashMap<Address, String>) {
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
        if inner.prune(Instant::now()) {
            self.broadcast(&inner);
        }
    }

    /// Works out which pair an advertisement is from, and decrypts it with
    /// that pair's ENC_KEY. Returns the pair's real MAC when identified.
    ///
    /// The address must resolve with a stored IRK, and the same pair's
    /// ENC_KEY decrypt the advertisement to a plaintext that passes the
    /// suffix check (#134). One key is tried, the resolving pair's: a
    /// decryption that passes by chance - 2^-23 per key tried - would show
    /// random levels for one of the user's pairs, and with every key tried on
    /// every advertisement, a crowd of foreign pairs or a spammer met that
    /// chance within hours. Resolving is 2^-24 per address and IRK, and an
    /// address lasts minutes ([`Coordinator::resolve`]).
    ///
    /// An address no stored IRK resolves counts only for a pair with no IRK
    /// stored yet, keyed before the keystore kept IRKs: its ENC_KEY is tried,
    /// as every key was before, until its next AAP connection brings its IRK.
    /// Otherwise the advertisement is dropped, decrypted with nothing.
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

        // Pairs with no IRK yet, the old way.
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
    /// One no IRK resolves is resolved again with each advertisement, as
    /// every key used to be tried on each: remembered, a spammer rotating its
    /// address would churn the map, and a foreign pair costs as little either
    /// way.
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
            primary_pod: Some(data.primary_pod()),
            // BLE carries no noise control mode; `snapshot` attaches the one
            // reported over AAP for the connected device.
            noise_mode: None,
            identity: Some(identity),
            current_ble_mac: Some(ble_mac),
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
        let now = Instant::now();
        // A worn pod whose partner is in the case has no lid to report, and
        // both advertise: replaced wholesale, the lid would blink between the
        // partner's reading and none several times a second. So it keeps the
        // lid the device last reported.
        if data.lid_open.is_none() && data.one_in_case {
            state.lid_open = inner
                .ble
                .get(&real_mac)
                .filter(|e| now.duration_since(e.last_seen) < BLE_CACHE_TTL)
                .and_then(|e| e.state.lid_open);
        }
        let live = Live::of(&state);
        // Recorded even while AAP supersedes it, so that the reading is
        // current the moment the link drops - and a linked device's
        // snapshot takes its live fields from it meanwhile.
        let previous = inner.ble.insert(
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
        let live_changed = previous.is_none_or(|e| Live::of(&e.state) != live);
        // AAP data for this device is exact and current; its own
        // advertisements lag behind. Nothing a snapshot shows has changed -
        // unless they told the link something new about the device,
        // typically its model on the first advertisement after connecting;
        // or changed what the snapshot takes from them, such as a pod taken
        // out of an ear; or the prune dropped something.
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
        // stored: re-pairing regenerates them, and a stale IRK or key leaves the
        // pair unidentified, so it would silently vanish from BLE. The reply is
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
            }
            // Without this the UI, tray and attachment never hear that the
            // connection ended and keep showing the last AAP reading.
            self.broadcast(&inner);
        }
        tracing::info!("AAP disconnected from {mac_addr} - showing its last BLE advertisement");
        true
    }

    /// Reads one link's packets until a read fails, and returns that error.
    /// It is handed the client rather than looking it up, so it only ever
    /// hears its own device.
    async fn aap_read_loop(&self, mac_addr: Address, client: &Arc<aap::Client>) -> anyhow::Error {
        tracing::debug!("AAP read loop started for {mac_addr}");
        loop {
            let packet = match client.read_packet().await {
                Ok(p) => p,
                Err(e) => return e,
            };

            // Redacted, as a key packet may be among them: logs get pasted
            // into issues.
            tracing::debug!(
                "AAP packet ({} bytes): {}",
                packet.len(),
                aap::keys::redacted_hex(&packet[..packet.len().min(8)])
            );

            match aap::decode(&packet) {
                Ok(Message::Battery(info)) => {
                    self.handle_battery_info(info, mac_addr, client).await;
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
                Ok(Message::Unknown { .. }) => {}
                Err(e) => tracing::warn!("AAP {e}"),
            }
        }
    }

    /// Makes a battery packet the link's reading, and broadcasts.
    ///
    /// AAP packets carry battery only. [`Inner::snapshot`] merges in the rest:
    /// the identity the link holds, the mode, and what the cached
    /// advertisement says about ears, lid, connection state and primary pod.
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
            primary_pod: None,
            noise_mode: None,
            identity: None,
            // AAP knows only the real address, and is always live.
            current_ble_mac: None,
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

    /// Records a mode the device reported on `client`'s link, and broadcasts if
    /// it changed.
    ///
    /// The startup dump repeats, and an unchanged mode is not worth waking the
    /// window, tray and attachment for. A stale report of the mode a
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
    /// Fails at once without a link, or with a link going; and once queued,
    /// when the send fails, or the link goes before the command is sent. All
    /// of those are a command not carried out. One the link went in the
    /// middle of, which may or may not have reached the device, fails with
    /// [`ConnectionLost`] instead.
    pub fn command(
        &self,
        mac_addr: Address,
        command: Command,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let (done, result) = oneshot::channel();
        let queued = match self.queues().get(&mac_addr) {
            Some(queue) => queue
                .commands
                .send(Queued { command, done })
                .map_err(|_| anyhow::anyhow!("AAP to {mac_addr} is closing")),
            None => Err(anyhow::anyhow!("no AAP connection to {mac_addr}")),
        };
        async move {
            queued.with_context(|| format!("{command} not sent"))?;
            // Dropped unanswered when the link went with the command still
            // in its queue; one under way answers for itself.
            result.await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "AAP to {mac_addr} went before the {command} was sent"
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
                command,
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
        }
    }

    /// Switches one connected device's noise control mode.
    ///
    /// The new mode is recorded optimistically. Nothing the device answers
    /// confirms it on every model: AirPods 4 sent a settings-changed
    /// notification that names neither the sub-command nor the mode, and the
    /// 0x0D echo carrying the mode back for one change in four; AirPods Pro 3
    /// echo every change, but only about a second later. Waiting would leave
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
        // Recent firmware treats Off as opt-in: a bare Off command gets an error
        // chime and no mode change. Enabling the setting is a change to the
        // device that outlives this session, so it is sent only when Off is what
        // was asked for - not flipped on at every connection. Idempotent, so
        // repeating it costs nothing.
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
    /// ENC_KEY is the one stored with it. With a new ENC_KEY - re-paired - the
    /// old IRK goes too, as it resolves none of the new pairing's addresses:
    /// without one, the pair is identified the old way rather than not at all.
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
    use crate::ble::decrypt::random_key;

    fn entry(age: Duration) -> Entry {
        Entry {
            state: PodState::empty(DataSource::Ble),
            last_seen: Instant::now() - age,
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
        }
    }

    /// A link as `open_aap` leaves it. The client never connects; building
    /// one does no I/O, and which address it holds does not matter here.
    fn link(reading: Option<PodState>) -> Link {
        Link {
            client: Arc::new(aap::Client::new(Address::any())),
            reading,
            noise_mode: ModeState::default(),
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
            keystore: Arc::new(Mutex::new(Persistence {
                keystore: Some(keystore_in(&dir)),
                saved: HashMap::new(),
            })),
            updates,
            #[cfg(feature = "harness")]
            aap_socket_dir: None,
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

    /// A reading outlives a few quiet minutes, so it is still there after the
    /// AirPods go silent in their case, and goes after [`BLE_CACHE_TTL`].
    #[test]
    fn prune_drops_only_stale_devices() {
        let (fresh, stale) = (AA, BB);
        let mut inner = inner_with(vec![
            (fresh, Duration::from_secs(10 * 60)),
            (stale, Duration::from_secs(31 * 60)),
        ]);
        assert!(inner.prune(Instant::now()));

        assert!(inner.ble.contains_key(&fresh));
        assert!(!inner.ble.contains_key(&stale));
        assert!(!inner.prune(Instant::now()), "nothing left to expire");
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
                last_seen: Instant::now(),
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
        let seen = Instant::now() - Duration::from_secs(90);
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
    /// device that link is up to. Attached in `snapshot` rather than carried on
    /// every battery packet, which is what dropped it in the Go version.
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
                .prune(Instant::now() + BLE_CACHE_TTL)
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
    /// advertisement - and drops it with that, rather than keeping a pod in
    /// an ear long after it may have come out.
    #[tokio::test]
    async fn a_device_on_aap_shows_in_ear_from_its_cached_advertisement() {
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
            (Some(true), Some(false))
        );
        assert_eq!(state.connection_state, Some(0x05));
        assert_eq!(state.primary_pod, Some(PodSide::Left));

        assert!(
            coordinator
                .inner
                .write()
                .await
                .prune(Instant::now() + BLE_CACHE_TTL)
        );
        let state = coordinator.inner.read().await.snapshot().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!((state.left_in_ear, state.right_in_ear), (None, None));
        assert_eq!(state.connection_state, None);
        assert_eq!(state.primary_pod, None);
        assert_eq!(state.identity, Some(PRO_3), "the link's, unlike these");
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
            device_names: HashMap::new(),
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

            // Longer than the fixed 500 ms that came before the requests until
            // they went unanswered on a pair that had only just connected.
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
    }
}
