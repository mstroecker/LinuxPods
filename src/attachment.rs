//! Device attachment: which pairs get an AAP link and a GNOME Settings battery.
//!
//! A pair is attached while BlueZ reports it connected and it lists the AirPods
//! service - see [`crate::bluez::is_airpods`]. Attaching opens its AAP link, and
//! detaching closes it and withdraws its battery. In between, the battery
//! mirrors the pair's reading from whichever source has one - see
//! [`battery_actions`].
//!
//! BlueZ is reached over the connection [`Attachment::new`] is given, so the
//! pipeline tests run against the harness's fake BlueZ the code the app runs
//! against bluetoothd.
//!
//! The battery is the only part that needs BlueZ to take the battery provider.
//! When it refuses, pairs are attached all the same, and get AAP - and their
//! keys, without which a pair new to the app never shows over BLE at all.
//!
//! # One record
//!
//! What attachment knows about a pair is one [`Attached`] entry, by real MAC: its
//! BlueZ object and the level its battery shows. Kept in two maps - the attached
//! pairs by object path, and the exported levels by MAC inside the battery
//! provider - the two had to be kept in step by hand.
//!
//! The coordinator's AAP links are not a copy of it. A link can fail to come up,
//! or die while BlueZ keeps the pair connected, so attached and linked are two
//! facts: attachment asks for a link, and the snapshot says whether there is one.
//!
//! # Per-device work
//!
//! Starts in [`Attachment::start`] and stops in [`Attachment::stop`], and nowhere
//! else, so whatever runs for a pair has one place to begin and one to end. For
//! now that is the AAP link, and starting it is awaited: the event loop waits for
//! each connection to come up or fail.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use bluer::Address;
use futures_util::{Stream, StreamExt};
use tokio::sync::watch;
use zbus::Connection;

use crate::bluez::{BatteryAction, BatteryProvider, ConnectionEvent, Devices};
use crate::podstate::{Coordinator, Snapshot};

/// Runs attachment on the system bus, where BlueZ is. What `main.rs` spawns.
pub async fn run(coordinator: Arc<Coordinator>) {
    let conn = match Connection::system().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("failed to connect to the system bus: {e}");
            return;
        }
    };
    match Attachment::new(conn, coordinator).await {
        Ok(attachment) => attachment.run().await,
        Err(e) => tracing::warn!("{e:#}"),
    }
}

/// Attaches and detaches pairs as BlueZ reports them connecting and
/// disconnecting, and mirrors their batteries into GNOME Settings.
pub struct Attachment {
    coordinator: Arc<Coordinator>,
    devices: Devices,
    /// `None` when BlueZ refused it - see the module docs.
    provider: Option<BatteryProvider>,
    /// BlueZ's connection events, from before the startup sweep on.
    events: Pin<Box<dyn Stream<Item = ConnectionEvent> + Send + Sync>>,
    /// Every attached pair, by real MAC. A disconnect names the BlueZ object;
    /// the MAC is kept rather than looked up then, by when the device may be
    /// gone.
    attached: HashMap<Address, Attached>,
}

/// One attached pair - see the module docs.
struct Attached {
    /// Its BlueZ object, which connection events name and its battery points at.
    path: String,
    /// The level its GNOME Settings battery shows, or `None` while it has none -
    /// see [`battery_actions`].
    battery: Option<u8>,
}

impl Attachment {
    /// Starts attachment over `conn`, with whichever BlueZ answers there:
    /// registers the battery provider, watches for connections, and attaches
    /// the pairs that are connected already. Back from here, the startup sweep
    /// is over, and [`Attachment::run`] takes every connection from then on.
    ///
    /// Fails only when connections cannot be watched.
    pub async fn new(conn: Connection, coordinator: Arc<Coordinator>) -> Result<Self> {
        let provider = match BatteryProvider::with_connection(conn.clone()).await {
            Ok(provider) => Some(provider),
            Err(e) => {
                tracing::warn!("failed to create BlueZ battery provider: {e:#}");
                tracing::warn!("Battery won't appear in GNOME Settings, but the UI still works");
                None
            }
        };
        let devices = Devices::new(conn);

        // Subscribe before the sweep so a connection racing startup is not missed.
        let events = Box::pin(
            devices
                .watch_connections()
                .await
                .context("failed to watch device connections")?,
        );
        tracing::info!("watching for AirPods connections");

        let mut attachment = Self {
            coordinator,
            devices,
            provider,
            events,
            attached: HashMap::new(),
        };
        // Names for the device switcher. Read once up front so paired devices are
        // labelled before any of them connects, then again on every event - a rename
        // made while the app was running takes effect at the next one, and a newly
        // paired pair shows up with its services, which BlueZ fills in once pairing
        // is done.
        attachment.update_names().await;
        attachment.sweep().await;
        Ok(attachment)
    }

    /// Follows BlueZ's connection events and the coordinator's snapshots until
    /// both end.
    pub async fn run(mut self) {
        let mut updates = self.coordinator.subscribe();
        loop {
            tokio::select! {
                Some(event) = self.events.next() => self.handle(event).await,

                // A snapshot can bring a pair's reading, change it or take it away.
                Ok(()) = updates.changed() => {}

                else => break,
            }

            // After a connection event as well: it changes which pairs are attached,
            // and a pair whose AAP link failed may have a BLE reading already.
            self.sync_batteries(&mut updates).await;
        }
    }

    /// Attaches every pair that is connected already - every pair, not just one.
    async fn sweep(&mut self) {
        match self.devices.connected_airpods().await {
            Ok(paths) => {
                for path in paths {
                    self.attach(&path).await;
                }
            }
            Err(e) => tracing::warn!("failed to list connected devices: {e:#}"),
        }
    }

    /// Hands the coordinator every paired device's BlueZ alias.
    async fn update_names(&self) {
        self.coordinator
            .set_device_names(self.devices.device_aliases().await)
            .await;
    }

    /// Takes one connection event: a device connecting or disconnecting, or its
    /// services marking it as AirPods.
    async fn handle(&mut self, event: ConnectionEvent) {
        let path = event.device_path.as_str();
        let alias = self.devices.device_alias(path).await;
        tracing::debug!(
            "connection event: {path} connected={:?} alias={alias:?}",
            event.connected
        );

        self.update_names().await;

        let connected = match event.connected {
            Some(connected) => connected,
            // Only the services changed. That matters to a pair which
            // connected before they marked it as AirPods, not to one
            // already attached.
            None if self.attached_at(path).is_some() => return,
            None => self.devices.device_connected(path).await,
        };

        if connected {
            // Only react to AirPods, not every Bluetooth device on the system.
            if !self.devices.device_is_airpods(path).await {
                return;
            }
            tracing::info!("AirPods connected: {path}");
            self.attach(path).await;
        } else if let Some(mac) = self.attached_at(path) {
            tracing::info!("AirPods disconnected: {path}");
            self.detach(mac).await;
        }
    }

    /// The attached pair whose BlueZ object is at `path`.
    fn attached_at(&self, path: &str) -> Option<Address> {
        self.attached
            .iter()
            .find(|(_, pair)| pair.path == path)
            .map(|(&mac, _)| mac)
    }

    /// Attaches the pair at `path` and starts its work. Its battery waits for a
    /// reading - see `sync_batteries`.
    async fn attach(&mut self, path: &str) {
        let mac = match self.devices.device_address(path).await {
            Ok(mac) => mac,
            Err(e) => {
                tracing::warn!("could not read address for {path}: {e:#}");
                return;
            }
        };
        // The sweep and a connection event can both find a pair. Attached
        // already, it keeps its entry and the battery it shows.
        self.attached.entry(mac).or_insert_with(|| Attached {
            path: path.to_string(),
            battery: None,
        });
        self.start(mac).await;
    }

    /// Detaches a pair: stops its work and withdraws its battery. Any other pair
    /// keeps its own.
    async fn detach(&mut self, mac: Address) {
        let Some(pair) = self.attached.remove(&mac) else {
            return;
        };
        self.stop(mac).await;
        if pair.battery.is_some() {
            self.apply(&BatteryAction::Remove { mac }).await;
        }
    }

    /// Starts a pair's work: its AAP link, and with it the link's read loop.
    async fn start(&self, mac: Address) {
        if let Err(e) = self.coordinator.connect_aap(mac).await {
            tracing::warn!("failed to connect AAP to {mac}: {e:#}");
            tracing::warn!("Falling back to BLE for battery monitoring (approximate)");
        }
    }

    /// Stops what [`Attachment::start`] started.
    async fn stop(&self, mac: Address) {
        self.coordinator.disconnect_aap(mac).await;
    }

    /// Mirrors each attached pair's lowest earbud into GNOME Settings, from
    /// whichever source has it - AAP, or BLE when the link failed.
    async fn sync_batteries(&mut self, updates: &mut watch::Receiver<Snapshot>) {
        // Read out first: the borrow is a read lock every broadcast waits for, so it
        // must not live across the awaits below.
        let levels: HashMap<Address, u8> = {
            let snapshot = updates.borrow_and_update();
            self.attached
                .keys()
                .filter_map(|&mac| Some((mac, snapshot.states.get(&mac)?.lowest_earbud()?)))
                .collect()
        };

        for action in battery_actions(&self.attached, &levels) {
            let applied = self.apply(&action).await;
            let Some(pair) = self.attached.get_mut(&action.mac()) else {
                continue;
            };
            match action {
                BatteryAction::Add { percentage, .. }
                | BatteryAction::Update { percentage, .. } => {
                    if applied {
                        pair.battery = Some(percentage);
                    }
                }
                // Forgotten either way: a removal that fails found no object, or
                // removed it and failed to signal that.
                BatteryAction::Remove { .. } => pair.battery = None,
            }
        }
    }

    /// Carries out one battery change, logging what came of it. Returns whether
    /// it went through.
    async fn apply(&mut self, action: &BatteryAction) -> bool {
        // Without a provider nothing goes through, and nothing is logged: that
        // was said once, at startup.
        let Some(provider) = &mut self.provider else {
            return false;
        };
        if let Err(e) = provider.apply(action).await {
            tracing::warn!("failed to apply {action:?} to the BlueZ battery: {e:#}");
            return false;
        }
        match action {
            BatteryAction::Add {
                mac, percentage, ..
            } => tracing::info!("BlueZ battery for {mac} exported at {percentage}%"),
            BatteryAction::Remove { mac } => tracing::info!("BlueZ battery for {mac} withdrawn"),
            BatteryAction::Update { .. } => {}
        }
        true
    }
}

/// Decides how the GNOME Settings batteries change so that each attached pair
/// shows its reading, and one without a reading shows nothing.
///
/// `Battery1` has no "unknown". An object exported ahead of the first reading sat
/// at 0% - a real level, so GNOME Settings and anything else reading BlueZ
/// batteries saw a flat battery, for the whole connection if neither AAP nor a
/// decryptable advertisement came through - and one that outlived its reading kept
/// showing a stale level. Absence is the only honest way to say "unknown".
///
/// `levels` holds the reading of each attached pair that has one, by MAC. A
/// detached pair is not in `attached` at all: [`Attachment::detach`] withdraws its
/// battery as it goes. Sorted by MAC, so the order does not depend on the maps'.
fn battery_actions(
    attached: &HashMap<Address, Attached>,
    levels: &HashMap<Address, u8>,
) -> Vec<BatteryAction> {
    let mut actions: Vec<BatteryAction> = attached
        .iter()
        .filter_map(|(&mac, pair)| match (levels.get(&mac), pair.battery) {
            (Some(&percentage), None) => Some(BatteryAction::Add {
                mac,
                device_path: pair.path.clone(),
                percentage,
            }),
            // An unchanged level would only be signal traffic.
            (Some(&percentage), Some(shown)) if percentage != shown => {
                Some(BatteryAction::Update { mac, percentage })
            }
            (None, Some(_)) => Some(BatteryAction::Remove { mac }),
            _ => None,
        })
        .collect();
    actions.sort_by_key(BatteryAction::mac);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_A: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    const PATH_A: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
    const MAC_B: Address = Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    const PATH_B: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";

    fn pair(path: &str, battery: Option<u8>) -> Attached {
        Attached {
            path: path.to_string(),
            battery,
        }
    }

    /// Pair A attached, its battery showing `shown`.
    fn attached_a(shown: Option<u8>) -> HashMap<Address, Attached> {
        HashMap::from([(MAC_A, pair(PATH_A, shown))])
    }

    fn levels(entries: &[(Address, u8)]) -> HashMap<Address, u8> {
        entries.iter().copied().collect()
    }

    /// Nothing is exported until the first reading, and then at that level -
    /// never at a placeholder 0% ahead of it.
    #[test]
    fn first_reading_adds_the_battery_at_that_level() {
        assert!(battery_actions(&attached_a(None), &levels(&[])).is_empty());
        assert_eq!(
            battery_actions(&attached_a(None), &levels(&[(MAC_A, 73)])),
            [BatteryAction::Add {
                mac: MAC_A,
                device_path: PATH_A.to_string(),
                percentage: 73,
            }]
        );
    }

    /// A snapshot that leaves the level alone sends no PropertiesChanged.
    #[test]
    fn unchanged_level_is_left_alone() {
        assert!(battery_actions(&attached_a(Some(73)), &levels(&[(MAC_A, 73)])).is_empty());
    }

    #[test]
    fn changed_level_updates_the_battery() {
        assert_eq!(
            battery_actions(&attached_a(Some(73)), &levels(&[(MAC_A, 72)])),
            [BatteryAction::Update {
                mac: MAC_A,
                percentage: 72,
            }]
        );
    }

    /// The link dropped or the BLE reading expired, with the pair still
    /// connected: the last level would be stale, and there is no "unknown".
    #[test]
    fn reading_gone_withdraws_the_battery() {
        assert_eq!(
            battery_actions(&attached_a(Some(73)), &levels(&[])),
            [BatteryAction::Remove { mac: MAC_A }]
        );
    }

    /// Each pair is decided on its own, and the order is by MAC.
    #[test]
    fn pairs_are_decided_independently() {
        let attached = HashMap::from([
            (MAC_A, pair(PATH_A, Some(60))),
            (MAC_B, pair(PATH_B, Some(40))),
        ]);
        assert_eq!(
            battery_actions(&attached, &levels(&[(MAC_A, 50)])),
            [
                BatteryAction::Remove { mac: MAC_B },
                BatteryAction::Update {
                    mac: MAC_A,
                    percentage: 50,
                },
            ]
        );
    }
}
