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

    // Neither setting exists yet: the app always connects, and nothing watches
    // for a low battery.
    let settings_group = adw::PreferencesGroup::builder().title("General").build();
    for title in ["Auto-connect", "Battery notifications"] {
        settings_group.add(&unavailable_row(title));
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

/// A setting that is planned but not built, in Preferences and on the Control
/// page. Shown so the plan is visible, but switched off and greyed out: it
/// neither claims a state nor takes a click, and nothing listens to the switch.
/// Once a feature lands, its state comes with the snapshot and a change goes
/// out as a command, as noise control's does.
pub(super) fn unavailable_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle("Not available yet")
        .sensitive(false)
        .build();
    row.add_suffix(
        &gtk::Switch::builder()
            .active(false)
            .valign(gtk::Align::Center)
            .build(),
    );
    row
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
        let mut rows = self.rows.borrow_mut();
        for (mac, known) in row_order(snapshot) {
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

/// The devices to list, each with whether a key is stored for it. Known devices
/// first, in stable key order, then any other device we hold state for - one on
/// AAP whose key has not arrived yet. Undecryptable advertisements are never
/// stored, so strangers do not appear.
fn row_order(snapshot: &Snapshot) -> Vec<(&String, bool)> {
    let mut entries: Vec<(&String, bool)> = snapshot.known_keys.iter().map(|m| (m, true)).collect();
    let mut unknown: Vec<&String> = snapshot
        .states
        .keys()
        .filter(|m| !snapshot.known_keys.contains(m))
        .collect();
    unknown.sort();
    entries.extend(unknown.into_iter().map(|m| (m, false)));
    entries
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
        self.row.set_title(&row_title(mac, state));

        match state {
            Some(s) if !s.model_name.is_empty() => self.row.set_subtitle(&s.model_name),
            _ => self.row.set_subtitle(""),
        }

        let (text, css) = row_status(known, connected, state);
        self.status_label.set_text(&text);
        for class in ["success", "warning", "dim-label"] {
            self.status_label.remove_css_class(class);
        }
        self.status_label.add_css_class(css);

        // Keys can only be requested over an active AAP connection.
        self.request_button.set_sensitive(connected);
    }
}

/// The real address, and the rotating BLE one alongside it when they differ.
fn row_title(mac: &str, state: Option<&PodState>) -> String {
    match state {
        Some(s) if !s.current_ble_mac.is_empty() && s.current_ble_mac != mac => {
            format!("{mac} • BLE: {}", s.current_ble_mac)
        }
        _ => mac.to_string(),
    }
}

/// A row's status text, and the style class to draw it in.
fn row_status(known: bool, connected: bool, state: Option<&PodState>) -> (String, &'static str) {
    match (known, connected, state) {
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
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    const A: &str = "AA:AA:AA:AA:AA:AA";
    const B: &str = "BB:BB:BB:BB:BB:BB";
    const C: &str = "CC:CC:CC:CC:CC:CC";
    const D: &str = "DD:DD:DD:DD:DD:DD";

    /// A reading heard `age` ago.
    fn heard(age: Duration) -> PodState {
        PodState {
            last_seen: Some(Instant::now() - age),
            ..Default::default()
        }
    }

    /// Known devices keep their key order, even without a reading, and come
    /// before the rest; a device with both a key and a reading is listed once.
    #[test]
    fn rows_list_known_devices_first() {
        let snapshot = Snapshot {
            known_keys: vec![B.into(), C.into()],
            states: [D, A, C]
                .into_iter()
                .map(|m| (m.to_string(), PodState::default()))
                .collect(),
            ..Default::default()
        };
        let order: Vec<(&str, bool)> = row_order(&snapshot)
            .into_iter()
            .map(|(m, known)| (m.as_str(), known))
            .collect();
        assert_eq!(order, [(B, true), (C, true), (A, false), (D, false)]);
    }

    #[test]
    fn title_adds_a_differing_ble_address() {
        let rotating = PodState {
            current_ble_mac: D.into(),
            ..Default::default()
        };
        assert_eq!(row_title(A, Some(&rotating)), format!("{A} • BLE: {D}"));

        let same = PodState {
            current_ble_mac: A.into(),
            ..Default::default()
        };
        assert_eq!(row_title(A, Some(&same)), A);
        assert_eq!(row_title(A, Some(&PodState::default())), A);
        assert_eq!(row_title(A, None), A);
    }

    /// Connected wins over everything, even a missing key: the key is requested
    /// as the link comes up.
    #[test]
    fn status_shows_a_connection_first() {
        let reading = heard(Duration::from_secs(5 * 60));
        for (known, state) in [(true, Some(&reading)), (false, None), (true, None)] {
            assert_eq!(
                row_status(known, true, state),
                ("Connected".to_string(), "success")
            );
        }
    }

    #[test]
    fn status_flags_a_missing_key() {
        let reading = heard(Duration::from_secs(5));
        assert_eq!(
            row_status(false, false, Some(&reading)),
            ("No key".to_string(), "warning")
        );
    }

    /// A fresh reading is an advertisement in progress; an older cached one says
    /// how old it is instead.
    #[test]
    fn status_tells_a_live_advertisement_from_a_cached_one() {
        let fresh = heard(Duration::from_secs(5));
        assert_eq!(
            row_status(true, false, Some(&fresh)),
            ("Advertising".to_string(), "dim-label")
        );

        let cached = heard(Duration::from_secs(2 * 60));
        assert_eq!(
            row_status(true, false, Some(&cached)),
            ("Last seen 2 min ago".to_string(), "dim-label")
        );

        assert_eq!(
            row_status(true, false, None),
            ("Idle".to_string(), "dim-label")
        );
    }
}
