//! Device attachment: which pairs get an AAP link and a GNOME Settings battery.
//!
//! A pair is attached while BlueZ reports it connected and it lists the AirPods
//! service. Attaching opens its AAP link; its battery is mirrored into GNOME
//! Settings once it has a reading.

use std::collections::HashMap;
use std::sync::Arc;

use bluer::Address;
use futures_util::StreamExt;

use crate::bluez;
use crate::podstate::{self, Coordinator};

/// Registers the BlueZ battery provider and manages the AAP connection.
pub async fn run(coordinator: Arc<Coordinator>) {
    let mut provider = match bluez::BatteryProvider::new().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to create BlueZ battery provider: {e:#}");
            tracing::warn!("Battery won't appear in GNOME Settings, but the UI still works");
            return;
        }
    };

    // Subscribe before the initial sweep so a connection racing startup is not missed.
    let events = match provider.watch_connections().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to watch device connections: {e:#}");
            return;
        }
    };
    tokio::pin!(events);
    tracing::info!("watching for AirPods connections");

    // Names for the device switcher. Read once up front so paired devices are
    // labelled before any of them connects, then again on every event - a rename
    // made while the app was running takes effect at the next one, and a newly
    // paired pair shows up with its services, which BlueZ fills in once pairing
    // is done.
    coordinator
        .set_device_names(provider.device_aliases().await)
        .await;

    // The AirPods attached so far, by BlueZ object path, with their MAC. Kept
    // rather than looked up on disconnect, by when the device may be gone.
    let mut attached: HashMap<String, Address> = HashMap::new();

    // Attach to AirPods that are already connected - every pair, not just one.
    match provider.connected_airpods().await {
        Ok(paths) => {
            for device_path in paths {
                attach_device(&provider, &coordinator, &mut attached, &device_path).await;
            }
        }
        Err(e) => tracing::warn!("failed to list connected devices: {e:#}"),
    }

    let mut updates = coordinator.subscribe();

    loop {
        tokio::select! {
            Some(event) = events.next() => {
                let alias = provider.device_alias(&event.device_path).await;
                tracing::debug!(
                    "connection event: {} connected={:?} alias={alias:?}",
                    event.device_path,
                    event.connected
                );

                coordinator.set_device_names(provider.device_aliases().await).await;

                let connected = match event.connected {
                    Some(connected) => connected,
                    // Only the services changed. That matters to a pair which
                    // connected before they marked it as AirPods, not to one
                    // already attached.
                    None if attached.contains_key(&event.device_path) => continue,
                    None => provider.device_connected(&event.device_path).await,
                };

                if connected {
                    // Only react to AirPods, not every Bluetooth device on the system.
                    if !provider.device_is_airpods(&event.device_path).await {
                        continue;
                    }
                    tracing::info!("AirPods connected: {}", event.device_path);
                    attach_device(&provider, &coordinator, &mut attached, &event.device_path)
                        .await;
                } else if let Some(mac) = attached.remove(&event.device_path) {
                    // Only this device's link goes; any other pair keeps its own.
                    // Its battery object goes in the sync below.
                    tracing::info!("AirPods disconnected: {}", event.device_path);
                    coordinator.disconnect_aap(mac).await;
                }
            }

            // A snapshot can bring a pair's reading, change it or take it away.
            Ok(()) = updates.changed() => {}

            else => break,
        }

        // After a connection event as well: it changes which pairs are attached,
        // and a pair whose AAP link failed may have a BLE reading already.
        sync_batteries(&mut provider, &attached, &mut updates).await;
    }
}

/// Mirrors each attached pair's lowest earbud into GNOME Settings, from whichever
/// source has it - AAP, or BLE when the link failed. A pair without a reading has
/// no battery object at all; see `bluez::battery_actions`.
async fn sync_batteries(
    provider: &mut bluez::BatteryProvider,
    attached: &HashMap<String, Address>,
    updates: &mut tokio::sync::watch::Receiver<podstate::Snapshot>,
) {
    // Read out first: the borrow is a read lock every broadcast waits for, so it
    // must not live across the awaits below.
    let levels: HashMap<Address, u8> = {
        let snapshot = updates.borrow_and_update();
        attached
            .values()
            .filter_map(|&mac| Some((mac, snapshot.states.get(&mac)?.lowest_earbud()?)))
            .collect()
    };

    for action in bluez::battery_actions(attached, &levels, provider.exported()) {
        if let Err(e) = provider.apply(&action).await {
            tracing::warn!("failed to apply {action:?} to the BlueZ battery: {e:#}");
            continue;
        }
        match action {
            bluez::BatteryAction::Add {
                mac, percentage, ..
            } => tracing::info!("BlueZ battery for {mac} exported at {percentage}%"),
            bluez::BatteryAction::Remove { mac } => {
                tracing::info!("BlueZ battery for {mac} withdrawn")
            }
            bluez::BatteryAction::Update { .. } => {}
        }
    }
}

/// Opens an AAP connection for one device. Its battery object waits for a
/// reading - see `sync_batteries`.
async fn attach_device(
    provider: &bluez::BatteryProvider,
    coordinator: &Arc<Coordinator>,
    attached: &mut HashMap<String, Address>,
    device_path: &str,
) {
    let mac = match provider.device_address(device_path).await {
        Ok(mac) => mac,
        Err(e) => {
            tracing::warn!("could not read address for {device_path}: {e:#}");
            return;
        }
    };
    attached.insert(device_path.to_string(), mac);

    // Starts the link's read loop as well.
    if let Err(e) = coordinator.connect_aap(mac).await {
        tracing::warn!("failed to connect AAP to {mac}: {e:#}");
        tracing::warn!("Falling back to BLE for battery monitoring (approximate)");
    }
}
