//! LinuxPods - AirPods management for GNOME.
//!
//! GTK owns the main thread; a multi-threaded tokio runtime carries the BLE, AAP
//! and D-Bus work, and the two meet over async channels consumed on the GTK main
//! context.

use linuxpods::{ble, bluez, indicator, podstate, ui};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::{gio, glib};

use indicator::{Indicator, TrayActions};
use linuxpods::aap::NoiseMode;
use podstate::Coordinator;

const APP_ID: &str = "com.linuxpods.app";

/// Bridges tray clicks onto the GTK main context, and noise control onto tokio.
struct AppActions {
    window: async_channel::Sender<WindowCommand>,
    coordinator: Arc<Coordinator>,
    /// Needed because ksni calls back from its own thread, which is not
    /// necessarily inside the runtime.
    runtime: tokio::runtime::Handle,
}

enum WindowCommand {
    Present,
    Quit,
}

impl TrayActions for AppActions {
    fn show_window(&self) {
        let _ = self.window.send_blocking(WindowCommand::Present);
    }

    fn quit(&self) {
        let _ = self.window.send_blocking(WindowCommand::Quit);
    }

    fn set_noise_mode(&self, mac: String, mode: NoiseMode) {
        let coordinator = self.coordinator.clone();
        self.runtime.spawn(async move {
            if let Err(e) = coordinator.set_noise_control(&mac, mode).await {
                tracing::warn!("failed to set noise control from tray: {e:#}");
            }
        });
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=info".into()),
        )
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start tokio runtime: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    let coordinator = match runtime.block_on(Coordinator::new()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to create pod state coordinator: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    // Ours, not GTK's: the application rejects options it does not know, so the
    // flag is taken out of the list before it ever sees it.
    let mut minimized = false;
    let args: Vec<String> = std::env::args()
        .filter(|arg| {
            let ours = arg == "--minimized";
            minimized |= ours;
            !ours
        })
        .collect();

    let (window_tx, window_rx) = async_channel::unbounded::<WindowCommand>();

    // Background workers.
    runtime.spawn(ble_task(coordinator.clone()));
    runtime.spawn(bluez_task(coordinator.clone()));
    runtime.spawn(expiry_task(coordinator.clone()));
    runtime.spawn(tray_task(
        coordinator.clone(),
        Arc::new(AppActions {
            window: window_tx,
            coordinator: coordinator.clone(),
            runtime: runtime.handle().clone(),
        }),
    ));

    // The window's artwork and the About dialog's icon load from the bundle.
    ui::register_resources();

    let app = adw::Application::builder().application_id(APP_ID).build();
    let handle = runtime.handle().clone();

    // Closing the window only hides it (see ui::activate), so quitting is an
    // action of its own, for the main menu and Ctrl+Q.
    app.add_action_entries([gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build()]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    // Activation runs again whenever the app is launched while already running -
    // from the launcher entry, say - and the window from the first run is the one
    // to raise. Building a second would leave two windows on one coordinator.
    let window: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));

    app.connect_activate(move |app| {
        if let Some(win) = window.borrow().as_ref() {
            win.present();
            return;
        }

        let win = ui::activate(app, coordinator.clone(), handle.clone());
        *window.borrow_mut() = Some(win.clone());

        // --minimized hands the app to the tray: the window is built, which is
        // what keeps GTK running, but stays hidden until something asks for it.
        if !minimized {
            win.present();
        }

        // Tray commands arrive here, on the main context, where touching the
        // window is legal.
        let window_rx = window_rx.clone();
        let app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = window_rx.recv().await {
                match cmd {
                    WindowCommand::Present => win.present(),
                    WindowCommand::Quit => app.quit(),
                }
            }
        });
    });

    let code = app.run_with_args(&args);
    // Keep the runtime alive until GTK returns.
    drop(runtime);
    code
}

/// Feeds BLE advertisements into the coordinator.
async fn ble_task(coordinator: Arc<Coordinator>) {
    let scanner = match ble::scanner::Scanner::new().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to create BLE scanner: {e}");
            return;
        }
    };

    if let Err(e) = scanner.start_discovery().await {
        tracing::error!("failed to start BLE discovery: {e}");
        return;
    }

    let stream = match scanner.advertisements().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to subscribe to advertisements: {e}");
            return;
        }
    };
    tokio::pin!(stream);

    while let Some(advert) = stream.next().await {
        coordinator
            .handle_advertisement(advert.data, advert.ble_mac)
            .await;
    }
}

/// Expires cached BLE readings on a clock. Advertisements prune as they arrive,
/// but with no device in range none arrive, and a cached reading would never go.
async fn expiry_task(coordinator: Arc<Coordinator>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        coordinator.expire().await;
    }
}

/// Registers the BlueZ battery provider and manages the AAP connection.
async fn bluez_task(coordinator: Arc<Coordinator>) {
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
    // labelled before any of them connects, then again on every connection event -
    // that is when a newly paired device first shows up, and when a rename made
    // while the app was running takes effect.
    coordinator
        .set_device_names(provider.device_aliases().await)
        .await;

    // The AirPods attached so far, by BlueZ object path, with their MAC. Kept
    // rather than looked up on disconnect, by when the device may be gone.
    let mut attached: HashMap<String, String> = HashMap::new();

    // Attach to AirPods that are already connected - every pair, not just one.
    match provider.connected_airpods().await {
        Ok(paths) => {
            for device_path in paths {
                attach_device(&mut provider, &coordinator, &mut attached, &device_path).await;
            }
        }
        Err(e) => tracing::warn!("failed to list connected devices: {e:#}"),
    }

    let updates = coordinator.subscribe();

    loop {
        tokio::select! {
            Some(event) = events.next() => {
                let alias = provider.device_alias(&event.device_path).await;
                tracing::debug!(
                    "connection event: {} connected={} alias={alias:?}",
                    event.device_path,
                    event.connected
                );

                coordinator.set_device_names(provider.device_aliases().await).await;

                if event.connected {
                    // Only react to AirPods, not every Bluetooth device on the system.
                    if !alias.contains("AirPods") {
                        continue;
                    }
                    tracing::info!("AirPods connected: {}", event.device_path);
                    attach_device(&mut provider, &coordinator, &mut attached, &event.device_path)
                        .await;
                } else if let Some(mac) = attached.remove(&event.device_path) {
                    // Only this device's link goes; any other pair keeps its own.
                    tracing::info!("AirPods disconnected: {}", event.device_path);
                    coordinator.disconnect_aap(&mac).await;
                    if let Err(e) = provider.remove_battery(&mac).await {
                        tracing::warn!("failed to remove battery object for {mac}: {e:#}");
                    }
                }
            }

            // Mirror each attached pair's lowest earbud into GNOME Settings, from
            // whichever source has it - AAP, or BLE when the link failed.
            Ok(snapshot) = updates.recv() => {
                for mac in attached.values() {
                    let Some(level) = snapshot.states.get(mac).and_then(|s| s.lowest_earbud())
                    else {
                        continue;
                    };
                    if let Err(e) = provider.update_percentage(mac, level).await {
                        tracing::debug!("update BlueZ battery for {mac}: {e}");
                    }
                }
            }

            else => break,
        }
    }
}

/// Registers a battery object and opens an AAP connection for one device.
async fn attach_device(
    provider: &mut bluez::BatteryProvider,
    coordinator: &Arc<Coordinator>,
    attached: &mut HashMap<String, String>,
    device_path: &str,
) {
    let Ok(mac) = provider.device_address(device_path).await else {
        tracing::warn!("could not read address for {device_path}");
        return;
    };
    attached.insert(device_path.to_string(), mac.clone());

    if !provider.has_battery(&mac) {
        match provider.add_battery(&mac, device_path).await {
            Ok(()) => tracing::info!("Battery provider registered for {device_path}"),
            Err(e) => tracing::warn!("failed to add battery object: {e:#}"),
        }
    }

    // Starts the link's read loop as well.
    if let Err(e) = coordinator.connect_aap(&mac).await {
        tracing::warn!("failed to connect AAP to {mac}: {e:#}");
        tracing::warn!("Falling back to BLE for battery monitoring (approximate)");
    }
}

/// Runs the system tray and keeps it in sync with coordinator state.
async fn tray_task(coordinator: Arc<Coordinator>, actions: Arc<AppActions>) {
    let handle = match Indicator::new(actions).start().await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("system tray unavailable: {e}");
            return;
        }
    };

    let updates = coordinator.subscribe();
    while let Ok(snapshot) = updates.recv().await {
        indicator::apply_snapshot(&handle, &snapshot).await;
    }
}
