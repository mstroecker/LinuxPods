//! GTK4/libadwaita interface.
//!
//! Updates arrive from the coordinator over an async channel consumed on the GTK
//! main context. That is the only path into the UI: GTK types are !Send, so a
//! background task can never touch a widget directly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::ble::decode_connection_state;
use crate::podstate::{Coordinator, DataSource, PodState, Snapshot};

/// Mirrors ui.BatteryWidgets.
pub struct BatteryWidgets {
    pub left_level: gtk::LevelBar,
    pub right_level: gtk::LevelBar,
    pub case_level: gtk::LevelBar,
    pub left_label: gtk::Label,
    pub right_label: gtk::Label,
    pub case_label: gtk::Label,
    pub status_label: gtk::Label,
}

/// Assets live alongside the crate at the repo root.
fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(name)
}

/// Mirrors ui.Activate.
pub fn activate(
    app: &adw::Application,
    coordinator: std::sync::Arc<Coordinator>,
    runtime: tokio::runtime::Handle,
) -> adw::ApplicationWindow {
    let updates = coordinator.subscribe();
    let win = adw::ApplicationWindow::new(app);
    win.set_title(Some("LinuxPods"));
    win.set_default_size(400, 500);

    let (control, dev_group) = setup_ui(&win);
    let control = Rc::new(control);
    win.present();

    // Updates arrive over an async channel consumed on the main context. GTK types
    // are !Send, so this is the only legal way in - enforced at compile time.
    let device_rows: Rc<RefCell<HashMap<String, DeviceRow>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Which device the Control tab is showing, and the snapshot behind it, so a
    // switcher change can redraw without waiting for the next update.
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let last_snapshot: Rc<RefCell<Option<Snapshot>>> = Rc::new(RefCell::new(None));
    // Guards against the programmatic set_selected() below re-entering this handler.
    let syncing = Rc::new(std::cell::Cell::new(false));

    control
        .device_dropdown
        .connect_selected_notify(glib::clone!(
            #[strong]
            control,
            #[strong]
            selected,
            #[strong]
            last_snapshot,
            #[strong]
            syncing,
            move |dropdown| {
                if syncing.get() {
                    return;
                }
                // GTK_INVALID_LIST_POSITION on an empty model is out of range for the
                // MAC list, so the lookup below simply finds nothing.
                let idx = dropdown.selected() as usize;
                let mac = control.device_macs.borrow().get(idx).cloned();
                if let Some(mac) = mac {
                    *selected.borrow_mut() = Some(mac.clone());
                    if let Some(snapshot) = last_snapshot.borrow().as_ref() {
                        render_device(&control, snapshot, Some(&mac));
                    }
                }
            }
        ));

    glib::spawn_future_local(async move {
        while let Ok(snapshot) = updates.recv().await {
            // The switcher lists every device we hold a key for. That keeps the list
            // stable instead of flickering as advertisements arrive, and strangers
            // still cannot appear: an advertisement we could not decrypt is stored
            // under its random MAC, which never matches a stored key.
            let macs = snapshot.known_keys.clone();

            // Read the selection BEFORE touching the widget. Splicing the model makes
            // the dropdown emit selected-notify, and if that ran unguarded it would clobber
            // the user's choice with index 0 - which is why switching device appeared
            // to snap back to the AAP-connected one.
            let current = selected.borrow().clone();

            syncing.set(true);
            sync_device_list(&control, &macs, &snapshot);

            // Keep the current selection if it still exists, else prefer the
            // AAP-connected device, else the first known one.
            let chosen = current
                .filter(|m| macs.contains(m))
                .or_else(|| snapshot.connected_mac.clone().filter(|m| macs.contains(m)))
                .or_else(|| macs.first().cloned());
            *selected.borrow_mut() = chosen.clone();

            if let Some(mac) = &chosen {
                if let Some(idx) = macs.iter().position(|m| m == mac) {
                    control.device_dropdown.set_selected(idx as u32);
                }
            }
            syncing.set(false);

            render_device(&control, &snapshot, chosen.as_deref());
            update_device_rows(&dev_group, &device_rows, &snapshot, &coordinator, &runtime);
            *last_snapshot.borrow_mut() = Some(snapshot);
        }
    });

    win
}

/// Mirrors ui.setupUI.
fn setup_ui(win: &adw::ApplicationWindow) -> (ControlView, adw::PreferencesGroup) {
    let header_bar = adw::HeaderBar::new();

    let view_stack = adw::ViewStack::new();

    let view_switcher = adw::ViewSwitcher::builder()
        .stack(&view_stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header_bar.set_title_widget(Some(&view_switcher));

    let (control_box, control) = create_control_view();
    view_stack.add_titled_with_icon(
        &control_box,
        Some("control"),
        "Control",
        "audio-headphones-symbolic",
    );

    let (settings_box, dev_group) = create_settings_view();
    view_stack.add_titled_with_icon(
        &settings_box,
        Some("settings"),
        "Settings",
        "preferences-system-symbolic",
    );

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&view_stack));

    win.set_content(Some(&toolbar_view));

    (control, dev_group)
}

/// Everything in the Control tab the update loop needs to touch.
pub struct ControlView {
    pub battery: BatteryWidgets,
    /// The device switcher; hidden unless more than one device is identified.
    pub device_dropdown: gtk::DropDown,
    pub device_list: gtk::StringList,
    /// MAC per row in `device_list`, kept parallel so the display string can differ.
    pub device_macs: RefCell<Vec<String>>,
    /// The labels currently in the model. Kept alongside the MACs so a rename or a
    /// newly decoded model name refreshes the switcher even though the MAC list is
    /// unchanged.
    pub device_labels: RefCell<Vec<String>>,
    pub noise_group: adw::PreferencesGroup,
    pub features_group: adw::PreferencesGroup,
}

/// Mirrors ui.createControlView.
fn create_control_view() -> (gtk::Box, ControlView) {
    let control_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

    // Device switcher. Only shown when more than one device is identified, so the
    // common single-device case looks unchanged.
    //
    // A flat dropdown centered over the battery display rather than a boxed list
    // row: it reads as a scope selector for what is directly below it, costs half
    // the height of an AdwComboRow, and gives the device name the full width
    // instead of sharing it with a title. What it selects is evident from where it
    // sits, so the explanation lives in a tooltip rather than a subtitle.
    let device_list = gtk::StringList::new(&[]);
    let device_dropdown = gtk::DropDown::builder()
        .model(&device_list)
        .halign(gtk::Align::Center)
        .tooltip_text("Which AirPods these readings are from")
        .visible(false)
        .build();
    device_dropdown.add_css_class("flat");
    control_box.append(&device_dropdown);

    let battery_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(20)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build();

    let image_paths = [
        "left_airpod_pro3.png",
        "right_airpod_pro3.png",
        "airpod_case.png",
    ];

    let mut level_bars = Vec::with_capacity(3);
    let mut labels = Vec::with_capacity(3);

    for path in image_paths {
        let column_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .build();

        let image = gtk::Image::from_file(asset(path));
        image.set_pixel_size(64);
        column_box.append(&image);

        let battery_level = gtk::LevelBar::new();
        battery_level.set_mode(gtk::LevelBarMode::Continuous);
        battery_level.set_value(0.0);
        battery_level.set_size_request(100, 20);
        column_box.append(&battery_level);
        level_bars.push(battery_level);

        let percent_label = gtk::Label::new(Some("--"));
        percent_label.add_css_class("dim-label");
        column_box.append(&percent_label);
        labels.push(percent_label);

        battery_box.append(&column_box);
    }

    control_box.append(&battery_box);

    let status_label = gtk::Label::new(Some("Searching for AirPods..."));
    status_label.add_css_class("dim-label");
    status_label.set_margin_top(10);
    control_box.append(&status_label);

    // Vec -> named fields. Go indexed levelBars[0..2]; destructuring is checked.
    let mut bars = level_bars.into_iter();
    let mut labs = labels.into_iter();
    let widgets = BatteryWidgets {
        left_level: bars.next().unwrap(),
        right_level: bars.next().unwrap(),
        case_level: bars.next().unwrap(),
        left_label: labs.next().unwrap(),
        right_label: labs.next().unwrap(),
        case_label: labs.next().unwrap(),
        status_label,
    };

    // Noise Control
    let noise_control_group = adw::PreferencesGroup::builder()
        .title("Noise Control")
        .build();

    let options = [
        ("transparency", "Transparency", "Hear the world around you"),
        (
            "adaptive",
            "Adaptive",
            "Automatically adjusts to your environment",
        ),
        (
            "noise_cancelling",
            "Noise Cancelling",
            "Block out background noise",
        ),
        ("off", "Off", "Noise control disabled"),
    ];

    let mut first_button: Option<gtk::CheckButton> = None;
    for (i, (id, title, desc)) in options.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(*title)
            .subtitle(*desc)
            .build();

        let radio_button = gtk::CheckButton::new();
        if i == 0 {
            radio_button.set_active(true);
            first_button = Some(radio_button.clone());
        } else {
            radio_button.set_group(first_button.as_ref());
        }

        // The Go version captured `opt` by reference in a loop closure - a classic
        // footgun. Rust forces the move to be explicit.
        let id = id.to_string();
        let title = title.to_string();
        radio_button.connect_toggled(move |b| {
            if b.is_active() {
                println!("Noise Control changed to: {title} ({id})");
            }
        });

        row.add_prefix(&radio_button);
        row.set_activatable_widget(Some(&radio_button));
        noise_control_group.add(&row);
    }
    control_box.append(&noise_control_group);

    // Features
    let conversation_group = adw::PreferencesGroup::builder().title("Features").build();
    let conversation_row = adw::ActionRow::builder()
        .title("Conversation Awareness")
        .subtitle("Lower media volume when you start speaking")
        .build();

    let conversation_switch = gtk::Switch::builder()
        .active(false)
        .valign(gtk::Align::Center)
        .build();
    conversation_row.add_suffix(&conversation_switch);
    conversation_row.set_activatable_widget(Some(&conversation_switch));

    conversation_switch.connect_active_notify(|s| {
        if s.is_active() {
            println!("Conversation Awareness enabled");
        } else {
            println!("Conversation Awareness disabled");
        }
    });

    conversation_group.add(&conversation_row);
    control_box.append(&conversation_group);

    let view = ControlView {
        battery: widgets,
        device_dropdown,
        device_list,
        device_macs: RefCell::new(Vec::new()),
        device_labels: RefCell::new(Vec::new()),
        noise_group: noise_control_group,
        features_group: conversation_group,
    };

    (control_box, view)
}

/// Repopulates the switcher, preserving the current selection where possible.
fn sync_device_list(view: &ControlView, macs: &[String], snapshot: &Snapshot) {
    let labels = device_labels(macs, snapshot);
    if *view.device_macs.borrow() == macs && *view.device_labels.borrow() == labels {
        return; // nothing changed; leave the selection alone
    }

    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();

    let old_len = view.device_list.n_items();
    view.device_list.splice(0, old_len, &refs);
    *view.device_macs.borrow_mut() = macs.to_vec();
    *view.device_labels.borrow_mut() = labels;

    // More than one device is the only case worth showing a switcher for.
    view.device_dropdown.set_visible(macs.len() > 1);
}

/// Names one entry of the switcher, in descending order of usefulness: the BlueZ
/// alias (what the user named the device, and what the rest of the desktop shows),
/// then the model decoded from BLE, then the bare MAC.
///
/// The MAC is appended whenever a label would otherwise be ambiguous - two pairs of
/// the same model, or two devices the user gave the same name - since the entries
/// would otherwise be impossible to tell apart.
fn device_labels(macs: &[String], snapshot: &Snapshot) -> Vec<String> {
    let names: Vec<Option<String>> = macs
        .iter()
        .map(|mac| {
            snapshot.device_name(mac).map(str::to_string).or_else(|| {
                snapshot
                    .states
                    .get(mac)
                    .map(|s| s.model_name.clone())
                    .filter(|n| !n.is_empty())
            })
        })
        .collect();

    macs.iter()
        .zip(&names)
        .map(|(mac, name)| match name {
            Some(name) if names.iter().filter(|n| n.as_ref() == Some(name)).count() == 1 => {
                name.clone()
            }
            Some(name) => format!("{name} ({mac})"),
            None => mac.clone(),
        })
        .collect()
}

/// Draws one device, and gates the control sections on how the data arrived.
fn render_device(view: &ControlView, snapshot: &Snapshot, mac: Option<&str>) {
    let state = mac.and_then(|m| snapshot.states.get(m));
    let connected = mac.is_some() && mac == snapshot.connected_mac.as_deref();

    match state {
        Some(state) => {
            update_battery_display(&view.battery, state);
            // Noise control and features need an AAP connection; over BLE we can
            // read state but not command the device, so they are disabled.
            let interactive = state.source == DataSource::Aap;
            view.noise_group.set_sensitive(interactive);
            view.features_group.set_sensitive(interactive);
        }
        // Known device, nothing heard from it yet: show it as empty rather than
        // hiding it, so the switcher and the display agree. Distinguish a device
        // that is connected but has not reported yet from one simply out of range -
        // otherwise a live AAP link looks identical to a missing device.
        None => {
            let status = if connected {
                "Connected • waiting for data"
            } else {
                "No recent data"
            };
            clear_battery_display(&view.battery, status);
            view.noise_group.set_sensitive(false);
            view.features_group.set_sensitive(false);
        }
    }
}

/// Resets the display, with the reason shown in the status line.
fn clear_battery_display(w: &BatteryWidgets, status: &str) {
    for (level, label) in [
        (&w.left_level, &w.left_label),
        (&w.right_level, &w.right_label),
        (&w.case_level, &w.case_label),
    ] {
        level.set_value(0.0);
        label.set_text("--");
    }
    w.status_label.set_text(status);
}

/// Mirrors ui.createSettingsView. Returns the Development group so the update loop
/// can populate it - in Go this was captured by the callback closure instead.
fn create_settings_view() -> (gtk::Box, adw::PreferencesGroup) {
    let settings_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

    let settings_group = adw::PreferencesGroup::builder()
        .title("General")
        .description("Application preferences")
        .build();

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
    settings_box.append(&settings_group);

    let dev_group = adw::PreferencesGroup::builder()
        .title("Development")
        .description("Encryption keys for decrypting BLE advertisements")
        .build();
    settings_box.append(&dev_group);

    let about_group = adw::PreferencesGroup::builder().title("About").build();
    let about_row = adw::ActionRow::builder()
        .title("LinuxPods")
        .subtitle("Version 0.1.0")
        .build();
    about_group.add(&about_row);
    settings_box.append(&about_group);

    (settings_box, dev_group)
}

/// Mirrors the DeviceRow struct declared inside createSettingsView.
pub struct DeviceRow {
    row: adw::ActionRow,
    /// Connection state for this device: Connected / Advertising / Idle.
    status_label: gtk::Label,
    request_button: gtk::Button,
}

/// Mirrors the device-list half of the coordinator callback.
fn update_device_rows(
    dev_group: &adw::PreferencesGroup,
    device_rows: &Rc<RefCell<HashMap<String, DeviceRow>>>,
    snapshot: &Snapshot,
    coordinator: &std::sync::Arc<Coordinator>,
    runtime: &tokio::runtime::Handle,
) {
    let connected_mac = snapshot.connected_mac.as_deref();

    // Known devices first, in stable key order, then anything else we have heard
    // advertising. Unknown entries are the whole point of a Development section:
    // they are how you spot a device whose key you have not captured yet.
    let mut entries: Vec<(&String, bool)> = snapshot.known_keys.iter().map(|m| (m, true)).collect();
    let mut unknown: Vec<&String> = snapshot
        .states
        .keys()
        .filter(|m| !snapshot.known_keys.contains(m))
        .collect();
    unknown.sort();
    entries.extend(unknown.into_iter().map(|m| (m, false)));

    for (mac_addr, known) in entries {
        let state = snapshot.states.get(mac_addr);
        let mut rows = device_rows.borrow_mut();
        if !rows.contains_key(mac_addr) {
            let row = adw::ActionRow::builder().title(mac_addr).build();

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
            request_button.connect_clicked(glib::clone!(
                #[weak]
                request_button,
                move |_| {
                    request_button.set_sensitive(false);
                    request_button.set_label("Requesting...");

                    let (tx, rx) = async_channel::bounded(1);
                    let coord = coord.clone();
                    rt.spawn(async move {
                        let _ = tx.send(coord.request_encryption_keys().await).await;
                    });

                    glib::spawn_future_local(async move {
                        match rx.recv().await {
                            Ok(Ok(())) => request_button.set_label("Request Keys"),
                            Ok(Err(e)) => {
                                tracing::warn!("key request failed: {e}");
                                request_button.set_label("Error - Retry");
                            }
                            Err(_) => request_button.set_label("Error - Retry"),
                        }
                        request_button.set_sensitive(true);
                    });
                }
            ));

            dev_group.add(&row);
            rows.insert(
                mac_addr.clone(),
                DeviceRow {
                    row,
                    status_label,
                    request_button,
                },
            );
        }

        let dev_row = &rows[mac_addr];
        let connected = Some(mac_addr.as_str()) == connected_mac;

        // Show the rotating BLE address alongside the real one when they differ.
        let title = match state {
            Some(s) if !s.current_ble_mac.is_empty() && &s.current_ble_mac != mac_addr => {
                format!("{mac_addr} • BLE: {}", s.current_ble_mac)
            }
            _ => mac_addr.clone(),
        };
        dev_row.row.set_title(&title);

        match state {
            Some(s) if !s.model_name.is_empty() => dev_row.row.set_subtitle(&s.model_name),
            _ => dev_row.row.set_subtitle(""),
        }

        let (text, css) = match (known, connected, state.is_some()) {
            (_, true, _) => ("Connected", "success"),
            // Advertising but no stored key: visible, not yet decryptable. Its MAC
            // rotates, so these entries come and go until a key is captured.
            (false, _, _) => ("No key", "warning"),
            (true, _, true) => ("Advertising", "dim-label"),
            (true, _, false) => ("Idle", "dim-label"),
        };
        dev_row.status_label.set_text(text);
        for class in ["success", "warning", "dim-label"] {
            dev_row.status_label.remove_css_class(class);
        }
        dev_row.status_label.add_css_class(css);

        // Keys can only be requested over an active AAP connection.
        dev_row.request_button.set_sensitive(connected);
    }

    // Drop a row once its key is gone and it has stopped advertising.
    let mut rows = device_rows.borrow_mut();
    rows.retain(|mac, dev_row| {
        let keep = snapshot.known_keys.contains(mac) || snapshot.states.contains_key(mac);
        if !keep {
            dev_group.remove(&dev_row.row);
        }
        keep
    });
}

/// Mirrors ui.updateBatteryDisplay.
fn update_battery_display(w: &BatteryWidgets, state: &PodState) {
    fn set(level: &gtk::LevelBar, label: &gtk::Label, value: Option<u8>, suffix: &str) {
        match value {
            Some(v) => {
                level.set_value(f64::from(v) / 100.0);
                label.set_text(&format!("{v}%{suffix}"));
            }
            None => {
                level.set_value(0.0);
                label.set_text("--");
            }
        }
    }

    let flags = |charging: bool, in_ear: bool| {
        let mut s = String::new();
        if charging {
            s.push_str(" ⚡");
        }
        if in_ear {
            s.push_str(" 👂");
        }
        s
    };

    set(
        &w.left_level,
        &w.left_label,
        state.left_battery,
        &flags(state.left_charging, state.left_in_ear),
    );
    set(
        &w.right_level,
        &w.right_label,
        state.right_battery,
        &flags(state.right_charging, state.right_in_ear),
    );
    set(
        &w.case_level,
        &w.case_label,
        state.case_battery,
        &flags(state.case_charging, false),
    );

    w.status_label.set_text(&status_line(state));
}

/// The one-line summary under the battery display.
fn status_line(state: &PodState) -> String {
    let lid = if state.lid_open { "Open" } else { "Closed" };
    // What the AirPods are doing - playing, on a call - comes from the BLE
    // advertisement only, so it is absent whenever the reading came over AAP.
    let activity = state
        .connection_state
        .map(|s| format!(" • {}", decode_connection_state(s)))
        .unwrap_or_default();
    // Which protocol produced these numbers.
    let source = match state.source {
        DataSource::Aap => "AAP",
        DataSource::Ble => "BLE",
        DataSource::Unknown => "unknown",
    };
    // Prefer the decoded name; fall back to the raw id only for models we do not
    // recognise, and to a bare label when nothing has identified it yet.
    let model = if !state.model_name.is_empty() {
        state.model_name.clone()
    } else if state.device_model != 0 {
        format!("Unknown (0x{:04X})", state.device_model)
    } else {
        "AirPods".to_string()
    };

    format!("{model} • Lid: {lid}{activity} • Source: {source}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asset path is resolved at runtime and a missing file fails silently -
    /// gtk::Image::from_file just renders nothing. Moving the crate broke this once.
    #[test]
    fn assets_resolve_from_the_crate_root() {
        for name in [
            "left_airpod_pro3.png",
            "right_airpod_pro3.png",
            "airpod_case.png",
        ] {
            let path = asset(name);
            assert!(path.exists(), "missing asset: {}", path.display());
        }
    }

    #[test]
    fn status_line_shows_what_the_airpods_are_doing() {
        let state = PodState {
            source: DataSource::Ble,
            model_name: "AirPods Pro 3".into(),
            lid_open: true,
            connection_state: Some(0x05),
            ..Default::default()
        };
        assert_eq!(
            status_line(&state),
            "AirPods Pro 3 • Lid: Open • Music • Source: BLE"
        );
    }

    /// AAP carries no connection state, and a stale carried-forward value would be
    /// worse than none - so the segment is left out entirely.
    #[test]
    fn status_line_omits_activity_on_aap() {
        let state = PodState {
            source: DataSource::Aap,
            model_name: "AirPods Pro 3".into(),
            connection_state: None,
            ..Default::default()
        };
        assert_eq!(
            status_line(&state),
            "AirPods Pro 3 • Lid: Closed • Source: AAP"
        );
    }

    fn snapshot(names: &[(&str, &str)], models: &[(&str, &str)]) -> Snapshot {
        Snapshot {
            states: models
                .iter()
                .map(|(mac, model)| {
                    (
                        (*mac).to_string(),
                        PodState {
                            model_name: (*model).to_string(),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            device_names: names
                .iter()
                .map(|(mac, name)| ((*mac).to_string(), (*name).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn labels_prefer_the_bluetooth_name() {
        let macs = vec!["AA:BB:CC:DD:EE:FF".to_string()];
        let snap = snapshot(
            &[("AA:BB:CC:DD:EE:FF", "Marcel's AirPods Pro")],
            &[("AA:BB:CC:DD:EE:FF", "AirPods Pro 2")],
        );
        assert_eq!(device_labels(&macs, &snap), ["Marcel's AirPods Pro"]);
    }

    #[test]
    fn labels_fall_back_to_the_model_then_the_mac() {
        let macs = vec![
            "AA:BB:CC:DD:EE:FF".to_string(),
            "11:22:33:44:55:66".to_string(),
        ];
        let snap = snapshot(&[], &[("AA:BB:CC:DD:EE:FF", "AirPods Pro 2")]);
        assert_eq!(
            device_labels(&macs, &snap),
            ["AirPods Pro 2", "11:22:33:44:55:66"]
        );
    }

    /// Two devices sharing a name would be indistinguishable in the switcher, so
    /// only the ambiguous ones carry their MAC.
    #[test]
    fn duplicate_names_keep_the_mac() {
        let macs = vec![
            "AA:BB:CC:DD:EE:FF".to_string(),
            "11:22:33:44:55:66".to_string(),
            "77:88:99:AA:BB:CC".to_string(),
        ];
        let snap = snapshot(
            &[
                ("AA:BB:CC:DD:EE:FF", "AirPods Pro"),
                ("11:22:33:44:55:66", "AirPods Pro"),
                ("77:88:99:AA:BB:CC", "AirPods Max"),
            ],
            &[],
        );
        assert_eq!(
            device_labels(&macs, &snap),
            [
                "AirPods Pro (AA:BB:CC:DD:EE:FF)",
                "AirPods Pro (11:22:33:44:55:66)",
                "AirPods Max",
            ]
        );
    }
}
