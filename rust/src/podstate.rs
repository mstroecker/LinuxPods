//! Centralized AirPods state coordination.
//!
//! Port of internal/podstate. Coordinates two data sources and notifies consumers:
//!   - AAP (accurate, 1%) while an L2CAP connection is up
//!   - BLE advertisements (approximate, 10%) otherwise, or 1% when decryptable
//!
//! # Difference from the Go original
//!
//! The Go coordinator inserted into `deviceStates` keyed by MAC and never deleted
//! (coordinator.go:165, no `delete` anywhere in the package). When no stored key
//! decrypts an advertisement it falls back to the *randomized* BLE MAC, and those
//! rotate for privacy - so the map grew without bound for as long as the app ran.
//! Here every entry carries a `last_seen` and [`Inner::prune`] drops stale ones.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::aap;
use crate::ble::parser::{PodSide, ProximityData};
use crate::ble::decode_model_name;
use crate::ble::decrypt::decrypt_for_device;
use crate::keystore::Keystore;

/// How long a device may go unseen before its state is dropped. Bounds the map
/// against rotating BLE MACs.
pub const DEVICE_TTL: Duration = Duration::from_secs(120);

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

    pub lid_open: bool,

    pub device_model: u16,
    pub model_name: String,
    pub color: u8,
    pub primary_pod: PodSide,

    pub real_mac: String,
    pub current_ble_mac: String,

    pub encryption_key: Option<Vec<u8>>,
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
    pub connected_mac: Option<String>,
}

impl Snapshot {
    /// The device an AAP connection is up for, else any device present.
    pub fn primary(&self) -> Option<&PodState> {
        self.connected_mac
            .as_ref()
            .and_then(|m| self.states.get(m))
            .or_else(|| self.states.values().next())
    }
}

struct Entry {
    state: PodState,
    last_seen: Instant,
}

struct Inner {
    devices: HashMap<String, Entry>,
    encryption_keys: HashMap<String, Vec<u8>>,
    connected_mac: Option<String>,
}

impl Inner {
    /// Drops devices unseen for longer than `ttl`. This is what keeps rotating
    /// BLE MACs from accumulating forever.
    fn prune(&mut self, now: Instant, ttl: Duration) {
        self.devices
            .retain(|_, e| now.duration_since(e.last_seen) < ttl);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            states: self
                .devices
                .iter()
                .map(|(k, v)| (k.clone(), v.state.clone()))
                .collect(),
            connected_mac: self.connected_mac.clone(),
        }
    }
}

/// Coordinates sources and broadcasts snapshots.
pub struct Coordinator {
    inner: RwLock<Inner>,
    keystore: Mutex<Keystore>,
    /// Shared so a blocked read never blocks a concurrent send. bluer's
    /// SeqPacket takes &self for both send and recv, so this is safe.
    aap_client: Mutex<Option<Arc<aap::Client>>>,
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
                devices: HashMap::new(),
                encryption_keys: loaded,
                connected_mac: None,
            }),
            keystore: Mutex::new(keystore),
            aap_client: Mutex::new(None),
            subscribers: std::sync::Mutex::new(Vec::new()),
        }))
    }

    /// Registers a new consumer. Every subscriber receives every snapshot.
    ///
    /// Unbounded so a slow consumer can never stall a protocol read loop.
    pub fn subscribe(&self) -> async_channel::Receiver<Snapshot> {
        let (tx, rx) = async_channel::unbounded();
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
        subs.retain(|tx| !matches!(
            tx.try_send(snapshot.clone()),
            Err(async_channel::TrySendError::Closed(_))
        ));
    }

    pub async fn connected_mac(&self) -> Option<String> {
        self.inner.read().await.connected_mac.clone()
    }

    pub async fn device_count(&self) -> usize {
        self.inner.read().await.devices.len()
    }

    /// Stores a state under `mac`, prunes stale devices, and broadcasts.
    async fn publish(&self, mac: String, state: PodState) {
        let snapshot = {
            let mut inner = self.inner.write().await;
            let now = Instant::now();
            inner.devices.insert(mac, Entry { state, last_seen: now });
            inner.prune(now, DEVICE_TTL);
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
                tracing::debug!("BLE: identified {real_mac} (random MAC {ble_mac}) via key");
                return Some(real_mac.clone());
            }
        }

        if !keys.is_empty() {
            tracing::debug!("BLE: no stored key decrypted advertisement from {ble_mac}");
        }
        None
    }

    /// Handles one BLE advertisement.
    pub async fn handle_advertisement(&self, mut data: ProximityData, ble_mac: String) {
        // BLE is only a fallback; AAP is authoritative while connected.
        if self.inner.read().await.connected_mac.is_some() {
            return;
        }

        let real_mac = self.identify_and_decrypt(&mut data, &ble_mac).await;
        let key_mac = real_mac.clone().unwrap_or_else(|| ble_mac.clone());

        let encryption_key = self
            .inner
            .read()
            .await
            .encryption_keys
            .get(&key_mac)
            .cloned();

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
            device_model: data.device_model,
            model_name: decode_model_name(data.device_model),
            color: data.color,
            primary_pod: data.primary_pod(),
            real_mac: real_mac.unwrap_or_default(),
            current_ble_mac: ble_mac,
            encryption_key,
        };

        tracing::debug!(
            "BLE {} [{}]: left={:?} right={:?} case={:?} lid_open={} in_ear={}/{}",
            state.current_ble_mac,
            if data.has_decrypted { "decrypted 1%" } else { "cleartext 10%" },
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
            data.raw_data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
        );

        self.publish(key_mac, state).await;
    }

    /// Opens an AAP connection and performs the handshake sequence.
    pub async fn connect_aap(&self, mac_addr: &str) -> Result<()> {
        let mut client = aap::Client::new(mac_addr)?;
        client.connect().await?;
        client.handshake().await.context("failed to send handshake")?;

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

        *self.aap_client.lock().await = Some(Arc::new(client));
        self.inner.write().await.connected_mac = Some(mac_addr.to_string());

        tracing::info!("AAP connected to {mac_addr} - using accurate battery data (1%)");
        Ok(())
    }

    /// Clones the client out of the mutex so callers never hold the lock across
    /// an await. Holding it across `read_packet` deadlocked every other user of
    /// the connection.
    async fn client(&self) -> Option<Arc<aap::Client>> {
        self.aap_client.lock().await.clone()
    }

    pub async fn disconnect_aap(&self) {
        if let Some(client) = self.aap_client.lock().await.take() {
            // Wakes a read loop parked in recv so it can exit and drop its Arc.
            client.shutdown();
            tracing::info!("AAP disconnected - resuming BLE scanning");
        }
        self.inner.write().await.connected_mac = None;
    }

    /// Reads AAP packets until the connection drops.
    pub async fn aap_read_loop(&self, mac_addr: String) {
        loop {
            let Some(client) = self.client().await else { return };

            let packet = match client.read_packet().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("AAP read error: {e}");
                    self.disconnect_aap().await;
                    return;
                }
            };

            if aap::is_battery_packet(&packet) {
                match aap::parse_battery_packet(&packet) {
                    Ok(info) => self.handle_battery_info(info, &mac_addr).await,
                    Err(e) => tracing::warn!("AAP battery parse error: {e}"),
                }
            }

            if aap::is_key_packet(&packet) {
                if let Ok(keys) = aap::parse_proximity_keys(&packet) {
                    if let Some(enc) = aap::find_encryption_key(&keys) {
                        self.store_encryption_key(&mac_addr, enc).await;
                    }
                }
            }
        }
    }

    async fn handle_battery_info(&self, info: aap::BatteryInfo, mac_addr: &str) {
        let encryption_key = self
            .inner
            .read()
            .await
            .encryption_keys
            .get(mac_addr)
            .cloned();

        let state = PodState {
            source: DataSource::Aap,
            left_battery: info.left.map(|b| b.level),
            right_battery: info.right.map(|b| b.level),
            case_battery: info.case.map(|b| b.level),
            left_charging: info.left.is_some_and(|b| b.is_charging()),
            right_charging: info.right.is_some_and(|b| b.is_charging()),
            case_charging: info.case.is_some_and(|b| b.is_charging()),
            real_mac: mac_addr.to_string(),
            encryption_key,
            // AAP carries no in-ear, lid, model, colour or primary-pod data.
            ..Default::default()
        };

        tracing::debug!(
            "AAP battery: left={:?} right={:?} case={:?}",
            state.left_battery,
            state.right_battery,
            state.case_battery
        );

        self.publish(mac_addr.to_string(), state).await;
    }

    /// Persists a newly received ENC_KEY and refreshes the affected state.
    async fn store_encryption_key(&self, mac_addr: &str, key: &[u8]) {
        {
            let mut inner = self.inner.write().await;
            inner
                .encryption_keys
                .insert(mac_addr.to_string(), key.to_vec());
            if let Some(entry) = inner.devices.get_mut(mac_addr) {
                entry.state.encryption_key = Some(key.to_vec());
            }
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

    /// Asks the AirPods for their proximity keys. Requires an active connection.
    pub async fn request_encryption_keys(&self) -> Result<()> {
        let client = self
            .client()
            .await
            .context("no active AAP connection - connect to AirPods first")?;
        client.request_proximity_keys().await?;
        tracing::info!("Encryption key request sent");
        Ok(())
    }

    pub async fn has_encryption_keys(&self) -> bool {
        !self.inner.read().await.encryption_keys.is_empty()
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
            devices: devices
                .into_iter()
                .map(|(m, age)| (m.to_string(), entry(age)))
                .collect(),
            encryption_keys: HashMap::new(),
            connected_mac: None,
        }
    }

    #[test]
    fn prune_drops_only_stale_devices() {
        let mut inner = inner_with(vec![
            ("fresh", Duration::from_secs(1)),
            ("stale", Duration::from_secs(300)),
        ]);
        inner.prune(Instant::now(), DEVICE_TTL);

        assert!(inner.devices.contains_key("fresh"));
        assert!(!inner.devices.contains_key("stale"));
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
            inner.devices.insert(
                format!("random-mac-{i}"),
                Entry { state: PodState::default(), last_seen: now },
            );
            inner.prune(now, DEVICE_TTL);
        }

        // Only entries inside the 120s window survive.
        assert!(
            inner.devices.len() <= 5,
            "expected bounded map, got {} entries",
            inner.devices.len()
        );
    }

    #[test]
    fn lowest_earbud_handles_missing_values() {
        let mut s = PodState { left_battery: Some(80), right_battery: Some(60), ..Default::default() };
        assert_eq!(s.lowest_earbud(), Some(60));

        s.right_battery = None;
        assert_eq!(s.lowest_earbud(), Some(80));

        s.left_battery = None;
        assert_eq!(s.lowest_earbud(), None);
    }

    #[test]
    fn snapshot_primary_prefers_connected_device() {
        let mut states = HashMap::new();
        states.insert("aa".into(), PodState { device_model: 1, ..Default::default() });
        states.insert("bb".into(), PodState { device_model: 2, ..Default::default() });

        let snap = Snapshot { states, connected_mac: Some("bb".into()) };
        assert_eq!(snap.primary().unwrap().device_model, 2);
    }
}
