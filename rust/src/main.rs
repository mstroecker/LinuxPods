//! LinuxPods - AirPods management for GNOME.
//!
//! Port of cmd/gui/main.go. GTK owns the main thread; a multi-threaded tokio
//! runtime carries the BLE, AAP and D-Bus work, and the two meet over async
//! channels consumed on the GTK main context.

// Several ported helpers mirror the Go API surface but are not wired up yet
// (noise control, key/colour decoding, battery removal). Kept deliberately.
#![allow(dead_code)]

mod aap;
mod ble;
mod bluez;
mod indicator;
mod keystore;
mod podstate;
mod ui;

use std::sync::Arc;

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::glib;

use crate::indicator::{Indicator, NoiseMode, TrayActions};
use crate::podstate::Coordinator;

const APP_ID: &str = "com.linuxpods.app";

/// Bridges tray clicks onto the GTK main context.
struct AppActions {
    window: async_channel::Sender<WindowCommand>,
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

    fn set_noise_mode(&self, mode: NoiseMode) {
        // Protocol for setting noise control is not implemented yet; the Go
        // version only logged here too.
        tracing::info!("Noise mode changed from tray: {mode:?}");
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

    let (window_tx, window_rx) = async_channel::unbounded::<WindowCommand>();

    // Background workers.
    runtime.spawn(ble_task(coordinator.clone()));
    runtime.spawn(bluez_task(coordinator.clone()));
    runtime.spawn(tray_task(
        coordinator.clone(),
        Arc::new(AppActions { window: window_tx }),
    ));

    let app = adw::Application::builder().application_id(APP_ID).build();
    let handle = runtime.handle().clone();

    app.connect_activate(move |app| {
        let win = ui::activate(app, coordinator.clone(), handle.clone());

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

    let code = app.run();
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

    // Connect to AirPods that are already paired and connected.
    if let Ok(device_path) = provider.discover_airpods().await {
        if let Err(e) = provider.add_battery(0, &device_path).await {
            tracing::warn!("failed to add battery object: {e}");
        } else {
            tracing::info!("Battery provider registered for {device_path}");
        }

        if let Ok(mac) = provider.device_address(&device_path).await {
            match coordinator.connect_aap(&mac).await {
                Ok(()) => {
                    let coord = coordinator.clone();
                    tokio::spawn(async move { coord.aap_read_loop(mac).await });
                }
                Err(e) => {
                    tracing::warn!("failed to connect AAP: {e:#}");
                    tracing::warn!("Falling back to BLE for battery monitoring (approximate)");
                }
            }
        }
    }

    // Mirror the lowest earbud level into GNOME Settings.
    let updates = coordinator.subscribe();
    while let Ok(snapshot) = updates.recv().await {
        let Some(level) = snapshot.primary().and_then(|s| s.lowest_earbud()) else {
            continue;
        };
        if let Err(e) = provider.update_percentage(level).await {
            tracing::debug!("update BlueZ battery: {e}");
        }
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
