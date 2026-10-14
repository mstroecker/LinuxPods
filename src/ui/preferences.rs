//! The Preferences dialog, and the Development group the update loop fills.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use super::{RECENT, last_seen_text};
use crate::podstate::{Coordinator, PodState, Snapshot};

/// The Preferences dialog, and its Development group for the update loop to
/// fill. The group's Request Keys buttons send to `coordinator`, on `runtime`.
pub(super) fn create_preferences_dialog(
    coordinator: Arc<Coordinator>,
    runtime: tokio::runtime::Handle,
) -> (adw::PreferencesDialog, DeviceRows) {
    let page = adw::PreferencesPage::new();

    let settings_group = adw::PreferencesGroup::builder().title("General").build();

    for (title, subtitle, active) in [
        (
            "Auto-connect",
            "Automatically connect when AirPods are detected",
            true,
        ),
        (
            "Battery notifications",
            "Show notification when battery is low",
            false,
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        let sw = gtk::Switch::builder()
            .active(active)
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&sw);
        row.set_activatable_widget(Some(&sw));
        settings_group.add(&row);
    }
    page.add(&settings_group);

    let dev_group = adw::PreferencesGroup::builder()
        .title("Development")
        .description("Encryption keys for decrypting BLE advertisements")
        .build();
    page.add(&dev_group);

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);

    let devices = DeviceRows {
        group: dev_group,
        rows: RefCell::default(),
        coordinator,
        runtime,
    };
    (dialog, devices)
}

/// The Development group: a row per device, by real MAC.
pub(super) struct DeviceRows {
    group: adw::PreferencesGroup,
    rows: RefCell<HashMap<String, DeviceRow>>,
    coordinator: Arc<Coordinator>,
    runtime: tokio::runtime::Handle,
}

impl DeviceRows {
    /// Adds a row for every device in the snapshot, redraws them all, and drops
    /// the rows of devices that are gone.
    pub(super) fn update(&self, snapshot: &Snapshot) {
        // Known devices first, in stable key order, then any other device we hold
        // state for - one on AAP whose key has not arrived yet. Undecryptable
        // advertisements are never stored, so strangers do not appear.
        let mut entries: Vec<(&String, bool)> =
            snapshot.known_keys.iter().map(|m| (m, true)).collect();
        let mut unknown: Vec<&String> = snapshot
            .states
            .keys()
            .filter(|m| !snapshot.known_keys.contains(m))
            .collect();
        unknown.sort();
        entries.extend(unknown.into_iter().map(|m| (m, false)));

        let mut rows = self.rows.borrow_mut();
        for (mac, known) in entries {
            let row = rows.entry(mac.clone()).or_insert_with(|| {
                let row = DeviceRow::new(mac, &self.coordinator, &self.runtime);
                self.group.add(&row.row);
                row
            });
            row.show(
                mac,
                snapshot.states.get(mac),
                known,
                snapshot.is_connected(mac),
            );
        }

        // Drop a row once its key is gone and it has stopped advertising.
        rows.retain(|mac, row| {
            let keep = snapshot.known_keys.contains(mac) || snapshot.states.contains_key(mac);
            if !keep {
                self.group.remove(&row.row);
            }
            keep
        });
    }
}

/// One device in the Development group.
struct DeviceRow {
    row: adw::ActionRow,
    /// Connection state for this device: Connected / Advertising / Idle.
    status_label: gtk::Label,
    request_button: gtk::Button,
}

impl DeviceRow {
    /// Builds the row, with its Request Keys button wired to the coordinator.
    fn new(mac: &str, coordinator: &Arc<Coordinator>, runtime: &tokio::runtime::Handle) -> Self {
        // Title and subtitle carry device data, which must never be read as
        // Pango markup - libadwaita's default.
        let row = adw::ActionRow::builder()
            .title(mac)
            .use_markup(false)
            .build();

        let status_label = gtk::Label::new(Some("Idle"));
        status_label.add_css_class("dim-label");
        status_label.set_valign(gtk::Align::Center);
        status_label.set_margin_end(8);
        row.add_suffix(&status_label);

        let request_button = gtk::Button::builder()
            .label("Request Keys")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        request_button.add_css_class("flat");
        row.add_suffix(&request_button);

        // The request runs on the tokio runtime because it does L2CAP I/O; the
        // result comes back over a channel and is applied with
        // spawn_future_local, which never leaves the main context and so keeps
        // the !Send widgets legal.
        let coord = coordinator.clone();
        let rt = runtime.clone();
        let mac = mac.to_string();
        request_button.connect_clicked(glib::clone!(
            #[weak]
            request_button,
            move |_| {
                request_button.set_sensitive(false);
                request_button.set_label("Requesting…");

                let (tx, rx) = async_channel::bounded(1);
                let coord = coord.clone();
                let mac = mac.clone();
                rt.spawn(async move {
                    let _ = tx.send(coord.request_encryption_keys(&mac).await).await;
                });

                glib::spawn_future_local(async move {
                    let failure = match rx.recv().await {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(format!("{e:#}")),
                        Err(_) => Some("the request task ended without replying".into()),
                    };
                    // A failure is an event, not a state of the button: it goes
                    // to a toast, and the button is ready to try again as is.
                    // The dialog it sits in carries its own toast overlay.
                    if let Some(reason) = failure {
                        tracing::warn!("key request failed: {reason}");
                        if let Some(dialog) = request_button
                            .ancestor(adw::PreferencesDialog::static_type())
                            .and_downcast::<adw::PreferencesDialog>()
                        {
                            dialog.add_toast(adw::Toast::new("Couldn’t request keys"));
                        }
                    }
                    request_button.set_label("Request Keys");
                    request_button.set_sensitive(true);
                });
            }
        ));

        DeviceRow {
            row,
            status_label,
            request_button,
        }
    }

    /// Draws the device's address, model and connection state. `known` is
    /// whether a key is stored for it.
    fn show(&self, mac: &str, state: Option<&PodState>, known: bool, connected: bool) {
        // Show the rotating BLE address alongside the real one when they differ.
        let title = match state {
            Some(s) if !s.current_ble_mac.is_empty() && s.current_ble_mac != mac => {
                format!("{mac} • BLE: {}", s.current_ble_mac)
            }
            _ => mac.to_string(),
        };
        self.row.set_title(&title);

        match state {
            Some(s) if !s.model_name.is_empty() => self.row.set_subtitle(&s.model_name),
            _ => self.row.set_subtitle(""),
        }

        let (text, css) = match (known, connected, state) {
            (_, true, _) => ("Connected".to_string(), "success"),
            // State without a stored key. Only a device on AAP has that - its key
            // is requested on connecting - and `Connected` above takes it first,
            // so this is a fallback.
            (false, _, _) => ("No key".to_string(), "warning"),
            // A cached reading is not an advertisement in progress.
            (true, _, Some(s)) => match s.last_seen.map(|t| t.elapsed()) {
                Some(age) if age >= RECENT => (last_seen_text(age), "dim-label"),
                _ => ("Advertising".to_string(), "dim-label"),
            },
            (true, _, None) => ("Idle".to_string(), "dim-label"),
        };
        self.status_label.set_text(&text);
        for class in ["success", "warning", "dim-label"] {
            self.status_label.remove_css_class(class);
        }
        self.status_label.add_css_class(css);

        // Keys can only be requested over an active AAP connection.
        self.request_button.set_sensitive(connected);
    }
}
