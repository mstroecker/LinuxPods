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
//! # Why entries expire
//!
//! When no stored key decrypts an advertisement, its state is keyed by the
//! *randomized* BLE MAC - and those rotate for privacy, several per minute per
//! device. Without eviction the map grows for as long as the app runs. Every entry
//! carries a `last_seen` and [`Inner::prune`] drops stale ones: identified devices
//! after [`BLE_CACHE_TTL`], the rest after [`DEVICE_TTL`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::aap;
use crate::ble::decode_model_name;
use crate::ble::decrypt::decrypt_for_device;
use crate::ble::parser::{PodSide, ProximityData};
use crate::keystore::Keystore;

/// How long an unidentified device may go unseen before its state is dropped.
/// Bounds the map against rotating BLE MACs.
pub const DEVICE_TTL: Duration = Duration::from_secs(120);

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

    pub real_mac: String,
    pub current_ble_mac: String,

    /// True when this state is attributable to a known device: always for AAP, and
    /// for BLE only when a stored key decrypted the advertisement. Unidentified
    /// advertisements are deliberately kept out of the main UI.
    pub identified: bool,

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
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub states: HashMap<String, PodState>,
    /// Every device with a live AAP link, in the order they connected.
    pub connected_macs: Vec<String>,
    /// Every MAC we hold an encryption key for, sorted. Independent of whether the
    /// device is currently advertising or connected.
    pub known_keys: Vec<String>,
    /// BlueZ aliases keyed by uppercase MAC - the names the rest of the desktop
    /// shows for these devices. Empty until the BlueZ task reports them, and it
    /// stays empty when BlueZ is unreachable, so consumers need a fallback.
    pub device_names: HashMap<String, String>,
}

impl Snapshot {
    /// The BlueZ alias for `mac`, when one is known.
    pub fn device_name(&self, mac: &str) -> Option<&str> {
        self.device_names
            .get(&mac.to_uppercase())
            .map(String::as_str)
    }

    pub fn is_connected(&self, mac: &str) -> bool {
        self.connected_macs.iter().any(|m| m == mac)
    }

    /// The device that has been on AAP longest, else any identified device. The
    /// longest link rather than the newest, so a second pair connecting does not
    /// take over the tray and the battery reading.
    pub fn primary(&self) -> Option<&PodState> {
        self.connected_macs
            .iter()
            .find_map(|m| self.states.get(m))
            .or_else(|| self.states.values().find(|s| s.identified))
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
    /// Orders `Snapshot::connected_macs`.
    since: Instant,
}

struct Inner {
    /// The last advertisement per device - BLE only. The connected device's entry
    /// keeps updating underneath AAP, so it is current when the link drops.
    ble: HashMap<String, Entry>,
    /// One per live AAP link, keyed by real MAC.
    links: HashMap<String, Link>,
    encryption_keys: HashMap<String, Vec<u8>>,
    device_names: HashMap<String, String>,
    /// Noise control mode per real MAC, held outside the device entries.
    ///
    /// It arrives on its own schedule - the startup dump can precede the first
    /// battery packet, so the entry that would hold it may not exist yet - and
    /// keeping it here spares every battery packet from carrying the mode
    /// forward the way the model and colour have to. Only AAP reports it, so
    /// the map is bounded by the number of devices connected this session.
    noise_modes: HashMap<String, aap::NoiseMode>,
}

impl Inner {
    /// True when a live AAP link makes this device's BLE advertisement redundant.
    /// Per device: every other device's advertisements still count.
    fn supersedes_ble(&self, advertising_mac: &str) -> bool {
        self.links.contains_key(advertising_mac)
    }

    /// Drops BLE readings older than their TTL, returning whether any went. This
    /// is what keeps rotating BLE MACs from accumulating forever.
    fn prune(&mut self, now: Instant) -> bool {
        let before = self.ble.len();
        self.ble.retain(|_, e| {
            let ttl = if e.state.identified {
                BLE_CACHE_TTL
            } else {
                DEVICE_TTL
            };
            now.duration_since(e.last_seen) < ttl
        });
        self.ble.len() != before
    }

    fn snapshot(&self) -> Snapshot {
        let mut known_keys: Vec<String> = self.encryption_keys.keys().cloned().collect();
        known_keys.sort();

        let mut states: HashMap<String, PodState> = self
            .ble
            .iter()
            // Hidden even before the first AAP packet: a BLE reading would leave
            // the controls insensitive on a device we can command.
            .filter(|(mac, _)| !self.supersedes_ble(mac))
            .map(|(mac, entry)| {
                let mut state = entry.state.clone();
                state.last_seen = Some(entry.last_seen);
                (mac.clone(), state)
            })
            .collect();

        for (mac, link) in &self.links {
            let Some(reading) = &link.reading else {
                continue;
            };
            let mut state = reading.clone();
            // Only a device on AAP has a mode we can vouch for. A cached one
            // from an earlier session would sit there as a selected radio button
            // for a device we cannot command.
            state.noise_mode = self.noise_modes.get(mac).copied();
            states.insert(mac.clone(), state);
        }

        let mut links: Vec<(&String, &Link)> = self.links.iter().collect();
        links.sort_by_key(|(_, link)| link.since);

        Snapshot {
            states,
            connected_macs: links.into_iter().map(|(mac, _)| mac.clone()).collect(),
            known_keys,
            device_names: self.device_names.clone(),
        }
    }
}

/// Coordinates sources and broadcasts snapshots.
pub struct Coordinator {
    inner: RwLock<Inner>,
    keystore: Mutex<Keystore>,
    /// One sender per consumer. A single shared channel would NOT work here:
    /// async_channel is MPMC, so each snapshot would go to exactly one of the
    /// UI / BlueZ provider / tray rather than all three.
    subscribers: std::sync::Mutex<Vec<async_channel::Sender<Snapshot>>>,
}

impl Coordinator {
    /// Loads persisted encryption keys so BLE decryption works from first launch.
    pub async fn new() -> Result<Arc<Self>> {
        let mut keystore = Keystore::new().context("failed to create keystore")?;
        let loaded = match keystore.load() {
            Ok(keys) => {
                if !keys.is_empty() {
                    tracing::info!("Loaded {} encryption key(s) from disk", keys.len());
                }
                keys
            }
            Err(e) => {
                tracing::warn!("failed to load encryption keys from disk: {e}");
                HashMap::new()
            }
        };

        Ok(Arc::new(Self {
            inner: RwLock::new(Inner {
                ble: HashMap::new(),
                links: HashMap::new(),
                encryption_keys: loaded,
                device_names: HashMap::new(),
                noise_modes: HashMap::new(),
            }),
            keystore: Mutex::new(keystore),
            subscribers: std::sync::Mutex::new(Vec::new()),
        }))
    }

    /// Registers a new consumer. Every subscriber receives every snapshot.
    ///
    /// Unbounded so a slow consumer can never stall a protocol read loop.
    ///
    /// The current state is delivered immediately. Without that a subscriber which
    /// starts before the first advertisement sees nothing at all - the window came
    /// up blank whenever no device was connected or in range, even though the keys
    /// were already loaded from disk.
    pub fn subscribe(&self) -> async_channel::Receiver<Snapshot> {
        let (tx, rx) = async_channel::unbounded();

        // try_read rather than blocking: subscribe() is called from the GTK main
        // context, and at startup there is no contention anyway.
        if let Ok(inner) = self.inner.try_read() {
            let _ = tx.try_send(inner.snapshot());
        }

        self.subscribers
            .lock()
            .expect("subscriber list poisoned")
            .push(tx);
        rx
    }

    /// Fans a snapshot out to every subscriber, dropping any that have gone away.
    ///
    /// Uses try_send so no await happens while the std mutex is held; the channels
    /// are unbounded, so the only failure mode is a closed receiver.
    fn broadcast(&self, snapshot: Snapshot) {
        let mut subs = self.subscribers.lock().expect("subscriber list poisoned");
        subs.retain(|tx| {
            !matches!(
                tx.try_send(snapshot.clone()),
                Err(async_channel::TrySendError::Closed(_))
            )
        });
    }

    /// Replaces the BlueZ alias map and broadcasts if anything changed.
    ///
    /// Names are display-only, so a snapshot is only worth sending when they
    /// actually differ - the BlueZ task re-reads them on every connection event,
    /// and rebroadcasting an identical map would wake the UI, tray and battery
    /// provider for nothing.
    pub async fn set_device_names(&self, names: HashMap<String, String>) {
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
        ble_mac: &str,
    ) -> Option<String> {
        let encrypted = data.encrypted_portion()?.to_vec();
        let keys = self.inner.read().await.encryption_keys.clone();

        for (real_mac, key) in &keys {
            let Ok(decrypted) = decrypt_for_device(&encrypted, key, real_mac) else {
                continue;
            };
            if data.add_decrypted_data(&decrypted).is_ok() {
                tracing::debug!("BLE decryptable: {ble_mac} -> {real_mac} (key matched)");
                return Some(real_mac.clone());
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
    /// An AAP connection only supersedes BLE *for that one device*. Other AirPods
    /// keep advertising and must still be tracked, so the decision is made per
    /// device after identification rather than globally.
    pub async fn handle_advertisement(&self, mut data: ProximityData, ble_mac: String) {
        let real_mac = self.identify_and_decrypt(&mut data, &ble_mac).await;
        let identified = real_mac.is_some();
        let key_mac = real_mac.clone().unwrap_or_else(|| ble_mac.clone());

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
            device_model: data.device_model,
            model_name: decode_model_name(data.device_model),
            color: data.color,
            primary_pod: data.primary_pod(),
            real_mac: real_mac.unwrap_or_default(),
            current_ble_mac: ble_mac,
            identified,
            // Stamped from the entry in `snapshot`.
            last_seen: None,
        };

        tracing::debug!(
            "BLE {} [{}]: left={:?} right={:?} case={:?} lid_open={:?} in_ear={}/{}",
            state.current_ble_mac,
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
            "BLE {} raw: {}",
            state.current_ble_mac,
            data.raw_data
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );

        let snapshot = {
            let mut inner = self.inner.write().await;
            let now = Instant::now();
            // Recorded even while AAP supersedes it, so that the reading is
            // current the moment the link drops.
            inner.ble.insert(
                key_mac.clone(),
                Entry {
                    state,
                    last_seen: now,
                },
            );
            let pruned = inner.prune(now);
            // AAP data for this device is exact and current; its own
            // advertisements are coarser and lag behind, and nothing a snapshot
            // shows has changed - unless the prune dropped something.
            if inner.supersedes_ble(&key_mac) && !pruned {
                return;
            }
            inner.snapshot()
        };
        self.broadcast(snapshot);
    }

    /// Opens an AAP connection to one device, performs the handshake sequence and
    /// starts the link's read loop. Other devices' links are untouched, and a
    /// device that already has one keeps it.
    pub async fn connect_aap(self: &Arc<Self>, mac_addr: &str) -> Result<()> {
        if self.inner.read().await.links.contains_key(mac_addr) {
            tracing::debug!("AAP already connected to {mac_addr}");
            return Ok(());
        }

        let mut client = aap::Client::new(mac_addr)?;
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
            if inner.links.contains_key(mac_addr) {
                client.shutdown();
                tracing::debug!("AAP already connected to {mac_addr}; dropping the duplicate");
                return Ok(());
            }
            inner.links.insert(
                mac_addr.to_string(),
                Link {
                    client: client.clone(),
                    reading: None,
                    since: Instant::now(),
                },
            );
            inner.snapshot()
        };
        self.broadcast(snapshot);

        tokio::spawn(self.clone().aap_read_loop(mac_addr.to_string(), client));

        tracing::info!(
            "AAP connected to {mac_addr} - exact battery for this device; \
             devices without a link continue over BLE"
        );
        Ok(())
    }

    /// Clones a link's client out so callers never hold the lock across an
    /// await. Holding it across `read_packet` deadlocked every other user of the
    /// connection.
    async fn client(&self, mac_addr: &str) -> Option<Arc<aap::Client>> {
        self.inner
            .read()
            .await
            .links
            .get(mac_addr)
            .map(|l| l.client.clone())
    }

    /// Closes one device's AAP link. Any other device's link is untouched.
    pub async fn disconnect_aap(&self, mac_addr: &str) {
        self.drop_link(mac_addr, None).await;
    }

    /// Removes a link, broadcasting and returning true if there was one. With
    /// `only`, the link goes only while it is still that client's: a read loop
    /// reporting its socket dead must not take down a newer link to the device.
    async fn drop_link(&self, mac_addr: &str, only: Option<&Arc<aap::Client>>) -> bool {
        let snapshot = {
            let mut inner = self.inner.write().await;
            match inner.links.get(mac_addr) {
                Some(link) if only.is_none_or(|c| Arc::ptr_eq(&link.client, c)) => {}
                _ => return false,
            }
            // After the link goes its readings are no longer current; kept, they
            // left the UI reporting "Source: AAP" for a disconnected device. The
            // cached advertisement shows through in their place.
            if let Some(link) = inner.links.remove(mac_addr) {
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
    async fn aap_read_loop(self: Arc<Self>, mac_addr: String, client: Arc<aap::Client>) {
        tracing::debug!("AAP read loop started for {mac_addr}");
        loop {
            let packet = match client.read_packet().await {
                Ok(p) => p,
                Err(e) => {
                    // A link closed on purpose is already gone; only one that
                    // died on its own is worth a warning.
                    if self.drop_link(&mac_addr, Some(&client)).await {
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
                    Ok(info) => self.handle_battery_info(info, &mac_addr, &client).await,
                    Err(e) => tracing::warn!("AAP battery parse error: {e}"),
                }
            }

            // The device's own report: its startup dump, or a mode changed from
            // another device such as an iPhone.
            if aap::is_noise_mode_packet(&packet) {
                match aap::parse_noise_mode_packet(&packet) {
                    Ok(mode) => self.handle_noise_mode(mode, &mac_addr).await,
                    Err(e) => tracing::warn!("AAP noise control parse error: {e}"),
                }
            }

            if aap::is_key_packet(&packet) {
                match aap::parse_proximity_keys(&packet)
                    .and_then(|keys| aap::find_encryption_key(&keys))
                {
                    Ok(key) => self.store_encryption_key(&mac_addr, &key).await,
                    Err(e) => tracing::warn!("AAP key packet rejected: {e}"),
                }
            }
        }
    }

    async fn handle_battery_info(
        &self,
        info: aap::BatteryInfo,
        mac_addr: &str,
        client: &Arc<aap::Client>,
    ) {
        // AAP packets carry battery only - no model, colour or orientation. Carry
        // that identity forward from whatever BLE last saw for this device, so the
        // UI does not lose the device name the moment it connects. The previous
        // AAP reading covers a device that has not advertised yet.
        let identity = {
            let inner = self.inner.read().await;
            inner
                .ble
                .get(mac_addr)
                .map(|e| &e.state)
                .or(inner.links.get(mac_addr).and_then(|l| l.reading.as_ref()))
                .map(|s| (s.device_model, s.model_name.clone(), s.color, s.primary_pod))
        };
        let (device_model, model_name, color, primary_pod) = identity.unwrap_or_default();

        let state = PodState {
            source: DataSource::Aap,
            left_battery: info.left.and_then(|b| b.level),
            right_battery: info.right.and_then(|b| b.level),
            case_battery: info.case.and_then(|b| b.level),
            left_charging: info.left.is_some_and(|b| b.is_charging()),
            right_charging: info.right.is_some_and(|b| b.is_charging()),
            case_charging: info.case.is_some_and(|b| b.is_charging()),
            real_mac: mac_addr.to_string(),
            identified: true,
            device_model,
            model_name,
            color,
            primary_pod,
            // AAP carries no in-ear, lid or connection-state data; those stay at
            // their defaults.
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
            let Some(link) = inner
                .links
                .get_mut(mac_addr)
                .filter(|l| Arc::ptr_eq(&l.client, client))
            else {
                return;
            };
            link.reading = Some(state);
            inner.snapshot()
        };
        self.broadcast(snapshot);
    }

    /// Records a mode the device reported, and broadcasts if it changed.
    ///
    /// The startup dump repeats, and an unchanged mode is not worth waking the
    /// window, tray and battery provider for.
    async fn handle_noise_mode(&self, mode: aap::NoiseMode, mac_addr: &str) {
        let snapshot = {
            let mut inner = self.inner.write().await;
            if inner.noise_modes.get(mac_addr) == Some(&mode) {
                return;
            }
            inner.noise_modes.insert(mac_addr.to_string(), mode);
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
    pub async fn set_noise_control(&self, mac_addr: &str, mode: aap::NoiseMode) -> Result<()> {
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

        let snapshot = {
            let mut inner = self.inner.write().await;
            if !inner.links.contains_key(mac_addr) {
                return Ok(());
            }
            inner.noise_modes.insert(mac_addr.to_string(), mode);
            inner.snapshot()
        };
        tracing::info!("Noise control of {mac_addr} set to {mode}");
        self.broadcast(snapshot);
        Ok(())
    }

    /// Persists a newly received ENC_KEY and broadcasts the new key list.
    ///
    /// Keys are requested on every connection, so this is usually the key already
    /// held. That is skipped rather than rewriting the keystore each time - which
    /// also keeps a device repeating its key packet from keeping the disk busy.
    async fn store_encryption_key(&self, mac_addr: &str, key: &[u8]) {
        {
            let mut inner = self.inner.write().await;
            if inner
                .encryption_keys
                .get(mac_addr)
                .is_some_and(|k| k.as_slice() == key)
            {
                tracing::debug!("Encryption key for {mac_addr} unchanged");
                return;
            }
            inner
                .encryption_keys
                .insert(mac_addr.to_string(), key.to_vec());
        }

        let mut ks = self.keystore.lock().await;
        if let Err(e) = ks.set(mac_addr, key) {
            tracing::warn!("failed to cache encryption key: {e}");
        } else if let Err(e) = ks.save() {
            tracing::warn!("failed to save encryption key to disk: {e}");
        } else {
            tracing::info!("Stored encryption key for {mac_addr} ({} bytes)", key.len());
        }
        drop(ks);

        let snapshot = self.inner.read().await.snapshot();
        self.broadcast(snapshot);
    }

    /// Asks one pair of AirPods for their proximity keys. Requires its AAP link.
    pub async fn request_encryption_keys(&self, mac_addr: &str) -> Result<()> {
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

    fn inner_with(devices: Vec<(&str, Duration)>) -> Inner {
        Inner {
            ble: devices
                .into_iter()
                .map(|(m, age)| (m.to_string(), entry(age)))
                .collect(),
            links: HashMap::new(),
            encryption_keys: HashMap::new(),
            device_names: HashMap::new(),
            noise_modes: HashMap::new(),
        }
    }

    /// A link as `connect_aap` leaves it. The client never connects; building
    /// one does no I/O, and which address it holds does not matter here.
    fn link(reading: Option<PodState>) -> Link {
        Link {
            client: Arc::new(aap::Client::new("00:00:00:00:00:00").unwrap()),
            reading,
            since: Instant::now(),
        }
    }

    fn coordinator_with(inner: Inner) -> (Arc<Coordinator>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Arc::new(Coordinator {
            inner: RwLock::new(inner),
            keystore: Mutex::new(Keystore::with_dir(dir.path().to_path_buf()).unwrap()),
            subscribers: std::sync::Mutex::new(Vec::new()),
        });
        (coordinator, dir)
    }

    #[test]
    fn prune_drops_only_stale_devices() {
        let mut inner = inner_with(vec![
            ("fresh", Duration::from_secs(1)),
            ("stale", Duration::from_secs(300)),
        ]);
        assert!(inner.prune(Instant::now()));

        assert!(inner.ble.contains_key("fresh"));
        assert!(!inner.ble.contains_key("stale"));
        assert!(!inner.prune(Instant::now()), "nothing left to expire");
    }

    /// An identified device's last advertisement outlives the unidentified TTL,
    /// so a reading is still there after the AirPods go quiet in their case.
    #[test]
    fn identified_readings_are_cached_for_the_cache_ttl() {
        let now = Instant::now();
        let mut inner = inner_with(vec![]);
        for (mac, age) in [("recent", 10 * 60), ("expired", 31 * 60)] {
            inner.ble.insert(
                mac.into(),
                Entry {
                    state: PodState {
                        identified: true,
                        ..Default::default()
                    },
                    last_seen: now - Duration::from_secs(age),
                },
            );
        }

        inner.prune(now);
        assert!(inner.ble.contains_key("recent"));
        assert!(!inner.ble.contains_key("expired"));
    }

    /// The Go original had no eviction at all, so a run of rotating BLE MACs grew
    /// the map without bound. This is the regression test for that.
    #[test]
    fn rotating_ble_macs_do_not_accumulate() {
        let mut inner = inner_with(vec![]);
        let start = Instant::now();

        // 500 rotations, one every 30s - far past the TTL.
        for i in 0..500 {
            let now = start + Duration::from_secs(i * 30);
            inner.ble.insert(
                format!("random-mac-{i}"),
                Entry {
                    state: PodState::default(),
                    last_seen: now,
                },
            );
            inner.prune(now);
        }

        // Only entries inside the 120s window survive.
        assert!(
            inner.ble.len() <= 5,
            "expected bounded map, got {} entries",
            inner.ble.len()
        );
    }

    /// Regression: the window came up blank with no device connected and none in
    /// range, because nothing was broadcast until the first advertisement arrived.
    #[tokio::test]
    async fn subscribe_delivers_current_state_immediately() {
        let coordinator = Coordinator::new().await.expect("coordinator");
        let rx = coordinator.subscribe();

        let snapshot = rx
            .try_recv()
            .expect("a subscriber must receive the current state without waiting");

        // Whatever is on disk, the snapshot must carry the loaded key list so the
        // UI can render known devices before anything is heard over the air.
        assert_eq!(
            snapshot.known_keys.len(),
            coordinator.known_key_count().await
        );
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
            identified: true,
            ..Default::default()
        }
    }

    /// While AAP is up the device shows its exact reading, and its cached
    /// advertisement stays out of sight - AAP is always live.
    #[test]
    fn aap_hides_the_connected_devices_ble_reading() {
        let mut inner = inner_with(vec![]);
        inner.ble.insert(
            "aa".into(),
            Entry {
                state: ble_state(70),
                last_seen: Instant::now(),
            },
        );
        inner.links.insert("aa".into(), link(None));

        assert!(
            !inner.snapshot().states.contains_key("aa"),
            "before the first AAP packet the device waits for data"
        );

        inner.links.get_mut("aa").unwrap().reading = Some(aap_state(83));
        let snap = inner.snapshot();
        assert_eq!(snap.states["aa"].source, DataSource::Aap);
        assert_eq!(snap.states["aa"].left_battery, Some(83));
        assert_eq!(snap.states["aa"].last_seen, None);
    }

    /// The point of the cache: after a disconnect the last advertisement shows at
    /// once, instead of nothing until the device next advertises. Regression, too:
    /// a dropped link once left the UI reporting Source: AAP forever.
    #[test]
    fn disconnect_falls_back_to_the_cached_advertisement() {
        let seen = Instant::now() - Duration::from_secs(90);
        let mut inner = inner_with(vec![]);
        inner.ble.insert(
            "aa".into(),
            Entry {
                state: ble_state(70),
                last_seen: seen,
            },
        );
        inner.links.insert("aa".into(), link(Some(aap_state(83))));

        // What disconnect_aap does.
        inner.links.remove("aa");

        let snap = inner.snapshot();
        assert_eq!(snap.states["aa"].source, DataSource::Ble);
        assert_eq!(snap.states["aa"].left_battery, Some(70));
        assert_eq!(snap.states["aa"].last_seen, Some(seen));
    }

    /// Two pairs on AAP at once each show their own exact reading, and closing
    /// one link leaves the other alone.
    #[test]
    fn links_are_per_device() {
        let mut inner = inner_with(vec![]);
        inner.links.insert("aa".into(), link(Some(aap_state(83))));
        inner.links.insert("bb".into(), link(Some(aap_state(41))));

        let snap = inner.snapshot();
        assert_eq!(snap.states["aa"].left_battery, Some(83));
        assert_eq!(snap.states["bb"].left_battery, Some(41));
        assert!(snap.is_connected("aa") && snap.is_connected("bb"));

        inner.links.remove("aa");
        let snap = inner.snapshot();
        assert!(!snap.is_connected("aa"));
        assert!(!snap.states.contains_key("aa"));
        assert_eq!(snap.states["bb"].source, DataSource::Aap);
    }

    /// Connection order, which `Snapshot::primary` goes by.
    #[test]
    fn connected_macs_follow_connection_order() {
        let now = Instant::now();
        let mut inner = inner_with(vec![]);
        for (mac, age) in [("bb", 5), ("cc", 1), ("aa", 10)] {
            let mut l = link(None);
            l.since = now - Duration::from_secs(age);
            inner.links.insert(mac.into(), l);
        }
        assert_eq!(inner.snapshot().connected_macs, ["aa", "bb", "cc"]);
    }

    /// A read loop outliving its link - the device reconnected while it was
    /// parked in recv - must neither close the new link nor write into it.
    /// Regression: the old loop went on to read the new socket, and stored the
    /// key it found there under the old device's MAC.
    #[tokio::test]
    async fn a_stale_read_loop_cannot_touch_a_newer_link() {
        let stale = link(None).client;
        let mut inner = inner_with(vec![]);
        inner.links.insert("aa".into(), link(None));
        let (coordinator, _dir) = coordinator_with(inner);

        coordinator
            .handle_battery_info(aap::BatteryInfo::default(), "aa", &stale)
            .await;
        assert!(coordinator.inner.read().await.links["aa"].reading.is_none());

        assert!(!coordinator.drop_link("aa", Some(&stale)).await);
        assert!(coordinator.inner.read().await.links.contains_key("aa"));

        coordinator.disconnect_aap("aa").await;
        assert!(coordinator.inner.read().await.links.is_empty());
    }

    /// The mode is only reported over AAP, so it may only be shown for the
    /// device that link is up to. Attached in `snapshot` rather than carried on
    /// every battery packet, which is what dropped it in the Go version.
    #[test]
    fn snapshot_attaches_the_mode_to_the_connected_device_only() {
        let mut inner = inner_with(vec![("bb", Duration::from_secs(1))]);
        inner
            .noise_modes
            .insert("aa".into(), aap::NoiseMode::Adaptive);
        inner
            .noise_modes
            .insert("bb".into(), aap::NoiseMode::Transparency);
        inner.links.insert("aa".into(), link(Some(aap_state(80))));

        let snap = inner.snapshot();
        assert_eq!(snap.states["aa"].noise_mode, Some(aap::NoiseMode::Adaptive));
        assert_eq!(
            snap.states["bb"].noise_mode, None,
            "a device we cannot command must not show a selected mode"
        );

        // A battery packet replaces the whole state; the mode survives because it
        // never lived there.
        inner.links.get_mut("aa").unwrap().reading = Some(aap_state(79));
        assert_eq!(
            inner.snapshot().states["aa"].noise_mode,
            Some(aap::NoiseMode::Adaptive)
        );
    }

    /// A device that has not reported a mode yet gets no selection, rather than
    /// the first mode in the list looking active.
    #[test]
    fn snapshot_leaves_an_unreported_mode_unset() {
        let mut inner = inner_with(vec![]);
        inner.links.insert("aa".into(), link(Some(aap_state(80))));
        assert_eq!(inner.snapshot().states["aa"].noise_mode, None);
    }

    #[test]
    fn aap_supersedes_ble_only_for_the_connected_device() {
        let mut inner = inner_with(vec![]);
        // With nothing connected, everything is processed.
        assert!(!inner.supersedes_ble("aa"));

        inner.links.insert("aa".into(), link(None));
        // The connected device's own advertisements are dropped...
        assert!(inner.supersedes_ble("aa"));
        // ...but a second pair of AirPods keeps being tracked over BLE.
        assert!(!inner.supersedes_ble("bb"));
        // An unidentified advertisement is keyed by its random MAC, so it is never
        // mistaken for the connected device.
        assert!(!inner.supersedes_ble("5C:4D:3F:B5:41:B6"));
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

    #[test]
    fn snapshot_primary_prefers_connected_device() {
        let mut states = HashMap::new();
        states.insert(
            "aa".into(),
            PodState {
                device_model: 1,
                ..Default::default()
            },
        );
        states.insert(
            "bb".into(),
            PodState {
                device_model: 2,
                ..Default::default()
            },
        );

        let snap = Snapshot {
            states,
            connected_macs: vec!["bb".into()],
            known_keys: vec!["aa".into(), "bb".into()],
            device_names: HashMap::new(),
        };
        assert_eq!(snap.primary().unwrap().device_model, 2);
    }

    /// Unidentified advertisements must not become the primary device: with
    /// rotating MACs, strangers nearby would otherwise drive the tray and the
    /// GNOME battery reading.
    #[test]
    fn primary_ignores_unidentified_devices() {
        let mut states = HashMap::new();
        states.insert(
            "known".into(),
            PodState {
                device_model: 7,
                identified: true,
                ..Default::default()
            },
        );
        states.insert(
            "stranger".into(),
            PodState {
                device_model: 9,
                identified: false,
                ..Default::default()
            },
        );

        let snap = Snapshot {
            states,
            connected_macs: vec![],
            known_keys: vec![],
            device_names: HashMap::new(),
        };
        assert_eq!(snap.primary().unwrap().device_model, 7);
    }

    #[test]
    fn primary_falls_back_to_an_identified_device() {
        let mut states = HashMap::new();
        states.insert(
            "stranger".into(),
            PodState {
                identified: false,
                ..Default::default()
            },
        );

        let snap = Snapshot {
            states,
            connected_macs: vec![],
            known_keys: vec![],
            device_names: HashMap::new(),
        };
        assert!(
            snap.primary().is_none(),
            "an unidentified device is not a primary"
        );
    }
}
