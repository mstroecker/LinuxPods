//! Mirror of internal/podstate types, plus a mock coordinator.
//!
//! The real coordinator fans updates out from BLE/AAP background workers. Here a
//! plain thread stands in for that, so the spike exercises the same cross-thread
//! path the real app needs.

use std::collections::HashMap;

/// Mirrors podstate.DataSource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSource {
    None,
    Ble,
    Aap,
}

/// Mirrors podstate.PodState. Go's `*int` for "unknown" becomes Option<u8>, which
/// is the one place the port is strictly better: nil-vs-zero can't be confused.
#[derive(Debug, Clone, Default)]
pub struct PodState {
    pub source: Option<DataSource>,

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

    pub real_mac: String,
    pub current_ble_mac: String,

    pub encryption_key: Option<Vec<u8>>,
}

/// What the coordinator hands the UI on every update: the full device map plus
/// which device currently holds an AAP connection.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub states: HashMap<String, PodState>,
    pub connected_mac: Option<String>,
}

/// Stand-in for PodStateCoordinator.RegisterCallback. Returns the receiving end of
/// an async channel; the UI drives it with glib::spawn_future_local.
pub fn spawn_mock_coordinator() -> async_channel::Receiver<Snapshot> {
    let (tx, rx) = async_channel::unbounded::<Snapshot>();

    std::thread::spawn(move || {
        let mac_a = "AA:BB:CC:DD:EE:FF".to_string();
        let mac_b = "11:22:33:44:55:66".to_string();
        let mut tick: u32 = 0;

        loop {
            tick += 1;
            let mut states = HashMap::new();

            // Connected device, AAP source, 1% accuracy - drains then recharges.
            let level = (100 - (tick * 3) % 100) as u8;
            states.insert(
                mac_a.clone(),
                PodState {
                    source: Some(DataSource::Aap),
                    left_battery: Some(level),
                    right_battery: Some(level.saturating_sub(4)),
                    case_battery: Some(level.saturating_sub(11)),
                    left_charging: tick % 4 == 0,
                    right_charging: tick % 4 == 0,
                    case_charging: false,
                    left_in_ear: tick % 3 != 0,
                    right_in_ear: tick % 5 != 0,
                    lid_open: tick % 6 < 3,
                    device_model: 0x2014,
                    model_name: "AirPods Pro 3".into(),
                    real_mac: mac_a.clone(),
                    current_ble_mac: format!("5{:X}:00:00:00:00:0{}", tick % 10, tick % 10),
                    encryption_key: Some(vec![0u8; 16]),
                },
            );

            // Second device appears after a few ticks, BLE-only, no key yet - this
            // is what drives the add/remove path in the Development group.
            if tick > 3 {
                states.insert(
                    mac_b.clone(),
                    PodState {
                        source: Some(DataSource::Ble),
                        left_battery: Some(55),
                        right_battery: Some(60),
                        case_battery: None,
                        model_name: "AirPods Pro 2".into(),
                        real_mac: mac_b.clone(),
                        current_ble_mac: "7A:11:22:33:44:55".into(),
                        device_model: 0x2014,
                        encryption_key: None,
                        ..Default::default()
                    },
                );
            }

            let snapshot = Snapshot {
                states,
                connected_mac: Some(mac_a.clone()),
            };

            if tx.send_blocking(snapshot).is_err() {
                return; // UI gone
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    });

    rx
}
