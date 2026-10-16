//! Centralized AirPods state coordination.
//!
//! Coordinates two data sources and notifies consumers:
//!   - AAP (exact) for every device an L2CAP connection is up to - several at once
//!   - BLE advertisements for every other device (10% steps, or exact once decrypted)
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
//! state is only ever the live link's and goes with it.
//!
//! # What is kept
//!
//! Only advertisements a stored key decrypts. That identifies the device, so its
//! state is keyed by the real MAC and the map stays bounded by the stored keys;
//! [`Inner::prune`] drops readings after [`BLE_CACHE_TTL`]. The rest are dropped on
//! arrival: keyed by a randomized MAC that rotates several times a minute, they
//! could never be attributed to anything, and keeping them - an entry and a
//! broadcast each - is exactly what a BLE spam flood feeds on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::Address;
use tokio::sync::{Mutex, RwLock, watch};

use crate::aap;
use crate::ble::decode_model_name;
use crate::ble::decrypt::decrypt_for_device;
use crate::ble::parser::{PodSide, ProximityData};
use crate::keystore::Keystore;

/// How long the last advertisement of an identified device stays on show. Keyed
/// by the real MAC, these do not rotate, so the map stays bounded by the number of
/// stored keys.
pub const BLE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataSource {
    #[default]
    Unknown,
    Ble,
    Aap,
}

impl std::fmt::Display for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ble => write!(f, "BLE"),
            Self::Aap => write!(f, "AAP"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Unified state, independent of which source produced it.
#[derive(Debug, Clone, Default)]
pub struct PodState {
    pub source: DataSource,

    pub left_battery: Option<u8>,
    pub right_battery: Option<u8>,
    pub case_battery: Option<u8>,

    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,

    pub left_in_ear: bool,
    pub right_in_ear: bool,

    /// `None` when nothing has reported it: over AAP, which carries no lid data, or
    /// from a BLE advertisement sent while the earbuds are out of the case.
    pub lid_open: Option<bool>,

    /// Decoded with `ble::decode_connection_state`. `None` while the reading came
    /// from AAP, which carries no such field - it is deliberately not carried
    /// forward from earlier BLE state the way the model and colour are, because
    /// unlike those it changes as the user plays music or takes a call, and a
    /// carried-forward value would sit there stale for the whole AAP session.
    pub connection_state: Option<u8>,

    /// Active noise control mode. `None` until the connected device reports one:
    /// BLE advertisements do not carry it, and a default would show the wrong
    /// mode as selected in the interface.
    pub noise_mode: Option<aap::NoiseMode>,

    pub device_model: u16,
    pub model_name: String,
    pub color: u8,
    pub primary_pod: PodSide,

    /// The randomized address the advertisement behind a BLE reading came
    /// from. `None` for AAP, which knows only the real one.
    pub current_ble_mac: Option<Address>,

    /// When the advertisement behind a BLE reading arrived; it may be a cached
    /// one up to [`BLE_CACHE_TTL`] old. `None` for AAP, which is always live.
    pub last_seen: Option<Instant>,
}

impl PodState {
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
    /// devices. Empty until the BlueZ task reports them, and it stays empty when
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
    /// Neither set takes in strangers: an advertisement no stored key decrypts is
    /// never stored, and an AAP link is only opened to attached AirPods. Nor does
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
    /// bluer's SeqPacket takes &self for both send and recv, so a read parked in
    /// the loop never blocks a command. A loop that looked its client up by MAC
    /// would, once the link was replaced, read the new socket under the old
    /// identity - which stored one device's key under another's MAC.
    client: Arc<aap::Client>,
    /// `None` until the first battery packet.
    reading: Option<PodState>,
    /// `None` until the device reports one or a command sets it. Kept apart
    /// from the reading, which every battery packet replaces, and merged in by
    /// [`Inner::snapshot`]. It goes with the link: a mode from an earlier
    /// session would show as selected after a reconnect, though it may have
    /// been changed from a phone meanwhile.
    noise_mode: Option<aap::NoiseMode>,
    /// What the device last advertised about itself, which AAP never says.
    /// Kept apart from the reading like the mode. Taken from the cached
    /// advertisement when the link comes up and refreshed by every one after,
    /// so it outlives that cache entry's expiry.
    identity: Option<Identity>,
    /// Orders `Snapshot::connected_macs`.
    since: Instant,
}

/// The parts of a reading that say which device it is rather than how it is
/// doing. Only advertisements carry them.
#[derive(Clone, PartialEq)]
struct Identity {
    device_model: u16,
    model_name: String,
    color: u8,
    primary_pod: PodSide,
}

impl Identity {
    fn of(state: &PodState) -> Self {
        Self {
            device_model: state.device_model,
            model_name: state.model_name.clone(),
            color: state.color,
            primary_pod: state.primary_pod,
        }
    }

    fn apply_to(&self, state: &mut PodState) {
        state.device_model = self.device_model;
        state.model_name.clone_from(&self.model_name);
        state.color = self.color;
        state.primary_pod = self.primary_pod;
    }
}

struct Inner {
    /// The last advertisement per device - BLE only. The connected device's entry
    /// keeps updating underneath AAP, so it is current when the link drops.
    ble: HashMap<Address, Entry>,
    /// One per live AAP link, keyed by real MAC.
    links: HashMap<Address, Link>,
    encryption_keys: HashMap<Address, Vec<u8>>,
    device_names: HashMap<Address, String>,
}

impl Inner {
    /// True when a live AAP link makes this device's BLE advertisement redundant.
    /// Per device: every other device's advertisements still count.
    fn supersedes_ble(&self, advertising_mac: Address) -> bool {
        self.links.contains_key(&advertising_mac)
    }

    /// The device's link, while it is still `client`'s. A packet read, or a
    /// command sent, just before the link was replaced belongs to the old
    /// session, and must not land on the new one.
    fn live_link(&mut self, mac_addr: Address, client: &Arc<aap::Client>) -> Option<&mut Link> {
        self.links
            .get_mut(&mac_addr)
            .filter(|l| Arc::ptr_eq(&l.client, client))
    }

    /// Records a mode on the link `client` belongs to, returning whether that
    /// changed anything.
    fn record_noise_mode(
        &mut self,
        mac_addr: Address,
        client: &Arc<aap::Client>,
        mode: aap::NoiseMode,
    ) -> bool {
        match self.live_link(mac_addr, client) {
            Some(link) if link.noise_mode != Some(mode) => {
                link.noise_mode = Some(mode);
                true
            }
            _ => false,
        }
    }

    /// Drops BLE readings older than [`BLE_CACHE_TTL`], returning whether any went.
    fn prune(&mut self, now: Instant) -> bool {
        let before = self.ble.len();
        self.ble
            .retain(|_, e| now.duration_since(e.last_seen) < BLE_CACHE_TTL);
        self.ble.len() != before
    }

    fn snapshot(&self) -> Snapshot {
        let mut known_keys: Vec<Address> = self.encryption_keys.keys().copied().collect();
        known_keys.sort_unstable();

        let mut states: HashMap<Address, PodState> = self
            .ble
            .iter()
            // Hidden even before the first AAP packet: a BLE reading would leave
            // the controls insensitive on a device we can command.
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
            state.noise_mode = link.noise_mode;
            // Merged here rather than copied into each battery packet: a
            // reading that arrived before the first advertisement kept a blank
            // model until the next packet, which over AAP comes only when a
            // level changes.
            if let Some(identity) = &link.identity {
                identity.apply_to(&mut state);
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

/// Where `Inner::encryption_keys` is saved, and what was last saved there.
struct Persistence {
    /// `None` when keys are not saved this session: the data directory could not
    /// be set up, or a key file that did not load could not be moved out of the
    /// way. Keys still arrive and decrypt BLE either way - from memory.
    keystore: Option<Keystore>,
    /// The keys as last loaded or saved successfully. A save is due whenever
    /// memory differs from this. Judged against memory instead, a key whose save
    /// failed looked unchanged the next time it arrived, and was never written.
    saved: HashMap<Address, Vec<u8>>,
}

/// Coordinates sources and broadcasts snapshots.
pub struct Coordinator {
    inner: RwLock<Inner>,
    /// Held across a save, so saves never overlap - they share a temporary file.
    keystore: Mutex<Persistence>,
    /// The latest snapshot, which every subscriber sees - see [`Coordinator::subscribe`].
    updates: watch::Sender<Snapshot>,
}

impl Coordinator {
    /// Loads persisted encryption keys so BLE decryption works from first launch.
    ///
    /// Without a usable data directory it starts anyway and keeps keys in memory
    /// only: a missing key costs BLE accuracy, nothing more.
    pub async fn new() -> Result<Arc<Self>> {
        let keystore = Keystore::new()
            .inspect_err(|e| tracing::warn!("encryption keys will not be saved: {e:#}"))
            .ok();
        Ok(Self::with_keystore(keystore))
    }

    /// [`Coordinator::new`] with the keystore given, so tests use their own
    /// directory rather than the user's.
    fn with_keystore(mut keystore: Option<Keystore>) -> Arc<Self> {
        let mut loaded = HashMap::new();
        if let Some(ks) = &keystore {
            match ks.load() {
                Ok(keys) => {
                    if !keys.is_empty() {
                        tracing::info!("Loaded {} encryption key(s) from disk", keys.len());
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
            encryption_keys: loaded,
            device_names: HashMap::new(),
        };
        let (updates, _) = watch::channel(inner.snapshot());

        Arc::new(Self {
            inner: RwLock::new(inner),
            keystore: Mutex::new(persistence),
            updates,
        })
    }

    /// Registers a new consumer, which sees the latest snapshot.
    ///
    /// A watch channel holds one value that every receiver sees, so the UI, tray
    /// and battery provider never compete for a snapshot, and one that falls
    /// behind skips stale snapshots rather than queueing them - an unbounded queue
    /// per consumer grew without limit under a flood of advertisements. Sending
    /// never waits, so no consumer can stall a protocol read loop either.
    ///
    /// Marked changed, so the current state arrives at once. Without that a
    /// subscriber which starts before the first advertisement sees nothing at all -
    /// the window came up blank whenever no device was connected or in range, even
    /// though the keys were already loaded from disk.
    ///
    /// Never hold the receiver's `borrow()` across an await: it is a read lock,
    /// and every broadcast waits for it.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        let mut rx = self.updates.subscribe();
        rx.mark_changed();
        rx
    }

    /// Replaces the snapshot every subscriber sees.
    fn broadcast(&self, snapshot: Snapshot) {
        self.updates.send_replace(snapshot);
    }

    /// Replaces the BlueZ alias map and broadcasts if anything changed.
    ///
    /// Names are display-only, so a snapshot is only worth sending when they
    /// actually differ - the BlueZ task re-reads them on every connection event,
    /// and rebroadcasting an identical map would wake the UI, tray and battery
    /// provider for nothing.
    pub async fn set_device_names(&self, names: HashMap<Address, String>) {
        let snapshot = {
            let mut inner = self.inner.write().await;
            if inner.device_names == names {
                return;
            }
            inner.device_names = names;
            inner.snapshot()
        };
        self.broadcast(snapshot);
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
        let snapshot = {
            let mut inner = self.inner.write().await;
            if !inner.prune(Instant::now()) {
                return;
            }
            inner.snapshot()
        };
        self.broadcast(snapshot);
    }

    /// Tries every stored key against the advertisement's encrypted portion.
    ///
    /// A key that validates identifies the device, letting us map a randomized
    /// BLE MAC back to the real one. Returns the real MAC when identified.
    pub async fn identify_and_decrypt(
        &self,
        data: &mut ProximityData,
        ble_mac: Address,
    ) -> Option<Address> {
        let encrypted = data.encrypted_portion()?.to_vec();
        let keys = self.inner.read().await.encryption_keys.clone();

        for (&real_mac, key) in &keys {
            let Ok(decrypted) = decrypt_for_device(&encrypted, key, real_mac) else {
                continue;
            };
            if data.add_decrypted_data(&decrypted).is_ok() {
                tracing::debug!("BLE decryptable: {ble_mac} -> {real_mac} (key matched)");
                return Some(real_mac);
            }
        }

        if !keys.is_empty() {
            tracing::debug!(
                "BLE not decryptable: {ble_mac} (tried {} stored key(s))",
                keys.len()
            );
        }
        None
    }

    /// Handles one BLE advertisement.
    ///
    /// One that no stored key decrypts is dropped here - see the module docs. The
    /// scanner's `BLE parsable:` log still records it.
    ///
    /// An AAP connection only supersedes BLE *for that one device*. Other AirPods
    /// keep advertising and must still be tracked, so the decision is made per
    /// device after identification rather than globally.
    pub async fn handle_advertisement(&self, mut data: ProximityData, ble_mac: Address) {
        let Some(real_mac) = self.identify_and_decrypt(&mut data, ble_mac).await else {
            return;
        };

        let state = PodState {
            source: DataSource::Ble,
            left_battery: data.left_battery,
            right_battery: data.right_battery,
            case_battery: data.case_battery,
            left_charging: data.left_charging,
            right_charging: data.right_charging,
            case_charging: data.case_charging,
            left_in_ear: data.left_in_ear,
            right_in_ear: data.right_in_ear,
            lid_open: data.lid_open,
            connection_state: Some(data.connection_state),
            // BLE carries no noise control mode; `snapshot` attaches the one
            // reported over AAP for the connected device.
            noise_mode: None,
            device_model: data.product_id,
            model_name: decode_model_name(data.product_id).map_or_else(
                || format!("Unknown (0x{:04X})", data.product_id),
                str::to_string,
            ),
            color: data.color,
            primary_pod: data.primary_pod(),
            current_ble_mac: Some(ble_mac),
            // Stamped from the entry in `snapshot`.
            last_seen: None,
        };

        tracing::debug!(
            "BLE {ble_mac} [{}]: left={:?} right={:?} case={:?} lid_open={:?} in_ear={}/{}",
            if data.has_decrypted {
                "decrypted 1%"
            } else {
                "cleartext 10%"
            },
            state.left_battery,
            state.right_battery,
            state.case_battery,
            state.lid_open,
            state.left_in_ear,
            state.right_in_ear
        );
        tracing::debug!(
            "BLE {ble_mac} raw: {}",
            data.raw_data
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );

        let identity = Identity::of(&state);
        let snapshot = {
            let mut inner = self.inner.write().await;
            let now = Instant::now();
            // Recorded even while AAP supersedes it, so that the reading is
            // current the moment the link drops.
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
                .is_some_and(|link| link.identity.replace(identity.clone()) != Some(identity));
            // AAP data for this device is exact and current; its own
            // advertisements are coarser and lag behind. Nothing a snapshot
            // shows has changed - unless they told the link something new
            // about the device, typically its model on the first advertisement
            // after connecting, or the prune dropped something.
            if inner.supersedes_ble(real_mac) && !identified && !pruned {
                return;
            }
            inner.snapshot()
        };
        self.broadcast(snapshot);
    }

    /// Opens an AAP connection to one device, performs the handshake sequence and
    /// starts the link's read loop. Other devices' links are untouched, and a
    /// device that already has one keeps it.
    pub async fn connect_aap(self: &Arc<Self>, mac_addr: Address) -> Result<()> {
        if self.inner.read().await.links.contains_key(&mac_addr) {
            tracing::debug!("AAP already connected to {mac_addr}");
            return Ok(());
        }

        let mut client = aap::Client::new(mac_addr);
        client.connect().await?;
        client
            .handshake()
            .await
            .context("failed to send handshake")?;

        // The Go version slept 500ms for the handshake to be processed.
        tokio::time::sleep(Duration::from_millis(500)).await;

        client
            .request_battery_status()
            .await
            .context("failed to request battery")?;
        client
            .enable_special_features()
            .await
            .context("failed to enable features")?;

        // Ask for the proximity keys on every connection, not only when none is
        // stored: re-pairing regenerates them, and a stale key would silently drop
        // BLE to 10% steps. The reply is stored by `aap_read_loop` like any key
        // packet, and waits in the socket until that loop starts. Without a key
        // only BLE accuracy suffers, so a failure does not fail the connection.
        if let Err(e) = client.request_proximity_keys().await {
            tracing::warn!("failed to request encryption keys: {e:#}");
        }

        let client = Arc::new(client);
        let snapshot = {
            let mut inner = self.inner.write().await;
            // Connecting takes a while, and another attempt may have got there
            // first. Its link stands; this one goes.
            if inner.links.contains_key(&mac_addr) {
                client.shutdown();
                tracing::debug!("AAP already connected to {mac_addr}; dropping the duplicate");
                return Ok(());
            }
            // Advertised before the link came up, if the cache still has it.
            let identity = inner.ble.get(&mac_addr).map(|e| Identity::of(&e.state));
            inner.links.insert(
                mac_addr,
                Link {
                    client: client.clone(),
                    reading: None,
                    // Reported by the startup dump, once the read loop is up.
                    noise_mode: None,
                    identity,
                    since: Instant::now(),
                },
            );
            inner.snapshot()
        };
        self.broadcast(snapshot);

        tokio::spawn(self.clone().aap_read_loop(mac_addr, client));

        tracing::info!(
            "AAP connected to {mac_addr} - exact battery for this device; \
             devices without a link continue over BLE"
        );
        Ok(())
    }

    /// Clones a link's client out so callers never hold the lock across an
    /// await. Holding it across `read_packet` deadlocked every other user of the
    /// connection.
    async fn client(&self, mac_addr: Address) -> Option<Arc<aap::Client>> {
        self.inner
            .read()
            .await
            .links
            .get(&mac_addr)
            .map(|l| l.client.clone())
    }

    /// Closes one device's AAP link. Any other device's link is untouched.
    pub async fn disconnect_aap(&self, mac_addr: Address) {
        self.drop_link(mac_addr, None).await;
    }

    /// Removes a link, broadcasting and returning true if there was one. With
    /// `only`, the link goes only while it is still that client's: a read loop
    /// reporting its socket dead must not take down a newer link to the device.
    async fn drop_link(&self, mac_addr: Address, only: Option<&Arc<aap::Client>>) -> bool {
        let snapshot = {
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
            }
            inner.snapshot()
        };
        tracing::info!("AAP disconnected from {mac_addr} - showing its last BLE advertisement");

        // Without this the UI, tray and battery provider never hear that the
        // connection ended and keep showing the last AAP reading.
        self.broadcast(snapshot);
        true
    }

    /// Reads one link's packets until its connection drops. It is handed the
    /// client rather than looking it up, so it only ever hears its own device.
    async fn aap_read_loop(self: Arc<Self>, mac_addr: Address, client: Arc<aap::Client>) {
        tracing::debug!("AAP read loop started for {mac_addr}");
        loop {
            let packet = match client.read_packet().await {
                Ok(p) => p,
                Err(e) => {
                    // A link closed on purpose is already gone; only one that
                    // died on its own is worth a warning.
                    if self.drop_link(mac_addr, Some(&client)).await {
                        tracing::warn!("AAP read error from {mac_addr}: {e}");
                    } else {
                        tracing::debug!("AAP read loop for {mac_addr} ended: {e}");
                    }
                    return;
                }
            };

            tracing::debug!(
                "AAP packet ({} bytes): {}",
                packet.len(),
                packet
                    .iter()
                    .take(8)
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );

            if aap::is_battery_packet(&packet) {
                match aap::parse_battery_packet(&packet) {
                    Ok(info) => self.handle_battery_info(info, mac_addr, &client).await,
                    Err(e) => tracing::warn!("AAP battery parse error: {e}"),
                }
            }

            // The device's own report: its startup dump, or a mode changed from
            // another device such as an iPhone.
            if aap::is_noise_mode_packet(&packet) {
                match aap::parse_noise_mode_packet(&packet) {
                    Ok(mode) => self.handle_noise_mode(mode, mac_addr, &client).await,
                    Err(e) => tracing::warn!("AAP noise control parse error: {e}"),
                }
            }

            if aap::is_key_packet(&packet) {
                match aap::parse_proximity_keys(&packet)
                    .and_then(|keys| aap::find_encryption_key(&keys))
                {
                    Ok(key) => self.store_encryption_key(mac_addr, &key).await,
                    Err(e) => tracing::warn!("AAP key packet rejected: {e}"),
                }
            }
        }
    }

    /// Makes a battery packet the link's reading, and broadcasts.
    ///
    /// AAP packets carry battery only - no model, colour or orientation. The
    /// link holds what the device advertised about itself, and
    /// [`Inner::snapshot`] merges it in.
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
            // AAP carries no in-ear, lid or connection-state data, nor a BLE
            // address; those stay at their defaults.
            ..Default::default()
        };

        tracing::debug!(
            "AAP battery: left={:?} right={:?} case={:?}",
            state.left_battery,
            state.right_battery,
            state.case_battery
        );

        let snapshot = {
            let mut inner = self.inner.write().await;
            // A packet read just before a disconnect must not bring back AAP
            // state for a link that is gone, nor land on a newer one.
            let Some(link) = inner.live_link(mac_addr, client) else {
                return;
            };
            link.reading = Some(state);
            inner.snapshot()
        };
        self.broadcast(snapshot);
    }

    /// Records a mode the device reported on `client`'s link, and broadcasts if
    /// it changed.
    ///
    /// The startup dump repeats, and an unchanged mode is not worth waking the
    /// window, tray and battery provider for.
    async fn handle_noise_mode(
        &self,
        mode: aap::NoiseMode,
        mac_addr: Address,
        client: &Arc<aap::Client>,
    ) {
        let snapshot = {
            let mut inner = self.inner.write().await;
            if !inner.record_noise_mode(mac_addr, client, mode) {
                return;
            }
            inner.snapshot()
        };
        tracing::info!("Noise control mode reported by {mac_addr}: {mode}");
        self.broadcast(snapshot);
    }

    /// Switches one connected device's noise control mode.
    ///
    /// The new mode is recorded optimistically. The device answers with a
    /// settings-changed notification that names neither the sub-command nor the
    /// mode, and the 0x0D echo carrying it back arrives only sometimes, so
    /// waiting for confirmation would leave the interface on the old mode for
    /// three modes out of four. A later report simply confirms what we set.
    pub async fn set_noise_control(&self, mac_addr: Address, mode: aap::NoiseMode) -> Result<()> {
        let client = self
            .client(mac_addr)
            .await
            .with_context(|| format!("no AAP connection to {mac_addr}"))?;
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

        let snapshot = {
            let mut inner = self.inner.write().await;
            // On the link the command went out on, and only while it lasts. One
            // that dropped and came back meanwhile is a new session, and the
            // device reports its mode there itself.
            if !inner.record_noise_mode(mac_addr, &client, mode) {
                return Ok(());
            }
            inner.snapshot()
        };
        self.broadcast(snapshot);
        Ok(())
    }

    /// Takes a newly received ENC_KEY into use, broadcasts the new key list, and
    /// persists it.
    ///
    /// Keys are requested on every connection, so this is usually the key already
    /// held and already saved. That is skipped rather than rewriting the keystore
    /// each time - which also keeps a device repeating its key packet from keeping
    /// the disk busy. "Already saved" goes by the last save that succeeded, so a
    /// key whose save failed is written again when it next arrives.
    async fn store_encryption_key(&self, mac_addr: Address, key: &[u8]) {
        let snapshot = {
            let mut inner = self.inner.write().await;
            let previous = inner.encryption_keys.insert(mac_addr, key.to_vec());
            (previous.as_deref() != Some(key)).then(|| inner.snapshot())
        };
        // Decrypting from here on, whether or not the key reaches the disk.
        if let Some(snapshot) = snapshot {
            self.broadcast(snapshot);
        }

        let mut persistence = self.keystore.lock().await;
        let Some(keystore) = persistence.keystore.clone() else {
            tracing::debug!("Encryption key for {mac_addr} kept for this session only");
            return;
        };
        let keys = self.inner.read().await.encryption_keys.clone();
        if keys == persistence.saved {
            tracing::debug!("Encryption key for {mac_addr} already saved");
            return;
        }

        // Off the async workers: the save waits on the disk.
        let saved = tokio::task::spawn_blocking(move || keystore.save(&keys).map(|()| keys))
            .await
            .unwrap_or_else(|e| Err(e.into()));
        match saved {
            Ok(keys) => {
                persistence.saved = keys;
                tracing::info!("Stored encryption key for {mac_addr} ({} bytes)", key.len());
            }
            Err(e) => tracing::warn!("failed to save encryption key to disk: {e:#}"),
        }
    }

    /// Asks one pair of AirPods for their proximity keys. Requires its AAP link.
    pub async fn request_encryption_keys(&self, mac_addr: Address) -> Result<()> {
        let client = self
            .client(mac_addr)
            .await
            .with_context(|| format!("no AAP connection to {mac_addr}"))?;
        client.request_proximity_keys().await?;
        tracing::info!("Encryption key request sent to {mac_addr}");
        Ok(())
    }

    pub async fn has_encryption_keys(&self) -> bool {
        !self.inner.read().await.encryption_keys.is_empty()
    }

    /// Number of devices we hold a key for.
    pub async fn known_key_count(&self) -> usize {
        self.inner.read().await.encryption_keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(age: Duration) -> Entry {
        Entry {
            state: PodState::default(),
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
            encryption_keys: HashMap::new(),
            device_names: HashMap::new(),
        }
    }

    /// A link as `connect_aap` leaves it. The client never connects; building
    /// one does no I/O, and which address it holds does not matter here.
    fn link(reading: Option<PodState>) -> Link {
        Link {
            client: Arc::new(aap::Client::new(Address::any())),
            reading,
            noise_mode: None,
            identity: None,
            since: Instant::now(),
        }
    }

    fn coordinator_with(inner: Inner) -> (Arc<Coordinator>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (updates, _) = watch::channel(inner.snapshot());
        let coordinator = Arc::new(Coordinator {
            inner: RwLock::new(inner),
            keystore: Mutex::new(Persistence {
                keystore: Some(keystore_in(&dir)),
                saved: HashMap::new(),
            }),
            updates,
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
        assert_eq!(coordinator.known_key_count().await, 0);
        coordinator.store_encryption_key(MAC, &KEY).await;

        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_str().unwrap().contains("keys.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), corrupt);
        assert_eq!(
            keystore_in(&dir).load().unwrap(),
            HashMap::from([(MAC, KEY.to_vec())]),
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
        coordinator.store_encryption_key(MAC, &KEY).await;
        chmod(&path, 0o600);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(coordinator.known_key_count().await, 1);
    }

    /// Regression (#38): "unchanged" was judged against memory, which already
    /// held the key whose save had failed, so the save was never retried.
    #[tokio::test]
    async fn a_failed_save_is_retried_when_the_key_arrives_again() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Coordinator::with_keystore(Some(keystore_in(&dir)));

        // Read-only, so the save cannot create its temporary file.
        chmod(dir.path(), 0o500);
        coordinator.store_encryption_key(MAC, &KEY).await;
        chmod(dir.path(), 0o700);
        assert!(keystore_in(&dir).load().unwrap().is_empty());
        assert_eq!(
            coordinator.known_key_count().await,
            1,
            "in use whether saved or not"
        );

        coordinator.store_encryption_key(MAC, &KEY).await;
        assert_eq!(keystore_in(&dir).load().unwrap()[&MAC], KEY);
    }

    /// Keys are requested on every connection; one already saved is not
    /// written again.
    #[tokio::test]
    async fn a_saved_key_is_not_written_again() {
        let (coordinator, dir) = coordinator_with(inner_with(vec![]));
        let path = dir.path().join("keys.json");
        coordinator.store_encryption_key(MAC, &KEY).await;
        assert!(path.exists());

        std::fs::remove_file(&path).unwrap();
        coordinator.store_encryption_key(MAC, &KEY).await;
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

    /// Undecryptable advertisements are not kept: keyed by a MAC that rotates
    /// every few minutes they can never be attributed to anything, and storing
    /// them - an entry and a broadcast each - is what a BLE spam flood fed on.
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

        for i in 0..1000 {
            coordinator.broadcast(Snapshot {
                known_keys: vec![nth(i)],
                ..Default::default()
            });
        }

        assert_eq!(rx.borrow_and_update().known_keys, [nth(999)]);
        assert!(!rx.has_changed().unwrap());
    }

    /// Regression: the window came up blank with no device connected and none in
    /// range, because nothing was broadcast until the first advertisement arrived.
    #[tokio::test]
    async fn subscribe_delivers_current_state_immediately() {
        let dir = tempfile::tempdir().unwrap();
        keystore_in(&dir)
            .save(&HashMap::from([(MAC, KEY.to_vec())]))
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
            source: DataSource::Aap,
            left_battery: Some(left),
            ..Default::default()
        }
    }

    fn ble_state(left: u8) -> PodState {
        PodState {
            source: DataSource::Ble,
            left_battery: Some(left),
            ..Default::default()
        }
    }

    /// While AAP is up the device shows its exact reading, and its cached
    /// advertisement stays out of sight - AAP is always live.
    #[test]
    fn aap_hides_the_connected_devices_ble_reading() {
        let mut inner = inner_with(vec![]);
        inner.ble.insert(
            AA,
            Entry {
                state: ble_state(70),
                last_seen: Instant::now(),
            },
        );
        inner.links.insert(AA, link(None));

        assert!(
            !inner.snapshot().states.contains_key(&AA),
            "before the first AAP packet the device waits for data"
        );

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

    /// Connection order, which `Snapshot::primary` goes by.
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
        aa.noise_mode = Some(aap::NoiseMode::Adaptive);
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
        let client = coordinator.client(AA).await.unwrap();
        coordinator
            .handle_noise_mode(aap::NoiseMode::Adaptive, AA, &client)
            .await;
        assert_eq!(
            coordinator.subscribe().borrow().states[&AA].noise_mode,
            Some(aap::NoiseMode::Adaptive)
        );

        assert!(coordinator.drop_link(AA, Some(&client)).await);
        // The reconnect, as `connect_aap` leaves it and the first battery
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

        assert_eq!(coordinator.inner.read().await.links[&AA].noise_mode, None);
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
        inner.encryption_keys.insert(MAC, KEY.to_vec());
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
    /// as a connected device sends it. Encrypted here, so no real key is needed.
    fn advertisement() -> ProximityData {
        use aes::cipher::{Block, BlockCipherEncrypt, KeyInit};

        let mut plain = [0u8; 16];
        plain[1..4].copy_from_slice(&[80, 80, 50]); // left, right, case
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
        assert_eq!(rx.borrow_and_update().states[&MAC].model_name, "");

        coordinator
            .handle_advertisement(advertisement(), BLE_MAC)
            .await;
        assert!(rx.has_changed().unwrap(), "the model is worth a broadcast");
        let state = rx.borrow_and_update().states[&MAC].clone();
        assert_eq!(state.source, DataSource::Aap);
        assert_eq!(state.left_battery, Some(83));
        assert_eq!(state.model_name, "AirPods Pro 3");
        assert_eq!(state.color, 0x01);

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
        assert_eq!(state.device_model, 0x2027);
        assert_eq!(state.model_name, "AirPods Pro 3");
        assert_eq!(state.color, 0x01);
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

        // The link as `connect_aap` opens it, to the address as BlueZ reports it.
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
        coordinator.store_encryption_key(reported, &KEY).await;
        assert_eq!(coordinator.known_key_count().await, 1);
        assert_eq!(coordinator.subscribe().borrow().devices(), [reported]);
    }

    #[test]
    fn aap_supersedes_ble_only_for_the_connected_device() {
        let mut inner = inner_with(vec![]);
        // With nothing connected, everything is processed.
        assert!(!inner.supersedes_ble(AA));

        inner.links.insert(AA, link(None));
        // The connected device's own advertisements are dropped...
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
            ..Default::default()
        };
        assert_eq!(s.lowest_earbud(), Some(60));

        s.right_battery = None;
        assert_eq!(s.lowest_earbud(), Some(80));

        s.left_battery = None;
        assert_eq!(s.lowest_earbud(), None);
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
            states: heard.iter().map(|&m| (m, PodState::default())).collect(),
            connected_macs: connected.to_vec(),
            known_keys: known.to_vec(),
            device_names: HashMap::new(),
        }
    }
}
