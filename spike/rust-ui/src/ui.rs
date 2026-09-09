//! Port of internal/ui/window.go.
//!
//! Structure and widget order deliberately match the Go original so the two can be
//! diffed side by side.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::state::{PodState, Snapshot};

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

/// Assets live at the repo root; the spike sits two directories below it.
fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets")
        .join(name)
}

/// Mirrors ui.Activate.
pub fn activate(app: &adw::Application, updates: async_channel::Receiver<Snapshot>) {
    let win = adw::ApplicationWindow::new(app);
    win.set_title(Some("LinuxPods"));
    win.set_default_size(400, 500);

    let (battery, dev_group) = setup_ui(&win);
    win.present();

    // Go used glib.IdleAdd from a goroutine. GTK types are !Send in Rust, so the
    // update instead arrives over an async channel consumed on the main context.
    // Same effect, enforced at compile time rather than by convention.
    let device_rows: Rc<RefCell<HashMap<String, DeviceRow>>> =
        Rc::new(RefCell::new(HashMap::new()));

    glib::spawn_future_local(async move {
        while let Ok(snapshot) = updates.recv().await {
            if let Some(state) = snapshot.states.values().next() {
                update_battery_display(&battery, state);
            }
            update_device_rows(&dev_group, &device_rows, &snapshot);
        }
    });
}

/// Mirrors ui.setupUI.
fn setup_ui(win: &adw::ApplicationWindow) -> (BatteryWidgets, adw::PreferencesGroup) {
    let header_bar = adw::HeaderBar::new();

    let view_stack = adw::ViewStack::new();

    let view_switcher = adw::ViewSwitcher::builder()
        .stack(&view_stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    header_bar.set_title_widget(Some(&view_switcher));

    let (control_box, battery) = create_control_view();
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

    (battery, dev_group)
}

/// Mirrors ui.createControlView.
fn create_control_view() -> (gtk::Box, BatteryWidgets) {
    let control_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

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
        ("adaptive", "Adaptive", "Automatically adjusts to your environment"),
        ("noise_cancelling", "Noise Cancelling", "Block out background noise"),
        ("off", "Off", "Noise control disabled"),
    ];

    let mut first_button: Option<gtk::CheckButton> = None;
    for (i, (id, title, desc)) in options.iter().enumerate() {
        let row = adw::ActionRow::builder().title(*title).subtitle(*desc).build();

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

    (control_box, widgets)
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
        ("Auto-connect", "Automatically connect when AirPods are detected", true),
        ("Battery notifications", "Show notification when battery is low", false),
    ] {
        let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
        let sw = gtk::Switch::builder().active(active).valign(gtk::Align::Center).build();
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
    key_label: gtk::Label,
    request_button: gtk::Button,
}

/// Mirrors the device-list half of the coordinator callback.
fn update_device_rows(
    dev_group: &adw::PreferencesGroup,
    device_rows: &Rc<RefCell<HashMap<String, DeviceRow>>>,
    snapshot: &Snapshot,
) {
    let connected_mac = snapshot.connected_mac.as_deref();

    for (mac_addr, state) in &snapshot.states {
        let mut rows = device_rows.borrow_mut();
        if !rows.contains_key(mac_addr) {
            let row = adw::ActionRow::builder().title(mac_addr).build();
            if !state.model_name.is_empty() {
                row.set_subtitle(&state.model_name);
            }

            let key_label = gtk::Label::new(Some("Not present"));
            key_label.add_css_class("dim-label");
            key_label.set_valign(gtk::Align::Center);
            key_label.set_margin_end(8);
            row.add_suffix(&key_label);

            let request_button = gtk::Button::builder()
                .label("Request Keys")
                .valign(gtk::Align::Center)
                .sensitive(false)
                .build();
            request_button.add_css_class("flat");
            row.add_suffix(&request_button);

            // Go spawned a goroutine here and hopped back via glib.IdleAdd. The
            // equivalent is spawn_future_local, which keeps !Send widgets legal
            // because it never leaves the main context.
            let mac = mac_addr.clone();
            request_button.connect_clicked(glib::clone!(
                #[weak]
                request_button,
                move |_| {
                    request_button.set_sensitive(false);
                    request_button.set_label("Requesting...");
                    let mac = mac.clone();
                    glib::spawn_future_local(async move {
                        glib::timeout_future_seconds(1).await;
                        println!("RequestEncryptionKeys() for {mac}");
                        request_button.set_label("Request Keys");
                        request_button.set_sensitive(true);
                    });
                }
            ));

            dev_group.add(&row);
            rows.insert(
                mac_addr.clone(),
                DeviceRow { row, key_label, request_button },
            );
        }

        let dev_row = &rows[mac_addr];

        let title = if Some(mac_addr.as_str()) == connected_mac {
            format!("{mac_addr} • Connected")
        } else if !state.current_ble_mac.is_empty() && &state.current_ble_mac != mac_addr {
            format!("{mac_addr} • BLE: {}", state.current_ble_mac)
        } else {
            mac_addr.clone()
        };
        dev_row.row.set_title(&title);

        if !state.model_name.is_empty() {
            dev_row.row.set_subtitle(&state.model_name);
        }

        match &state.encryption_key {
            Some(k) if !k.is_empty() => {
                dev_row.key_label.set_text("Present");
                dev_row.key_label.remove_css_class("dim-label");
                dev_row.key_label.add_css_class("success");
            }
            _ => {
                dev_row.key_label.set_text("Not present");
                dev_row.key_label.remove_css_class("success");
                dev_row.key_label.add_css_class("dim-label");
            }
        }

        dev_row
            .request_button
            .set_sensitive(Some(mac_addr.as_str()) == connected_mac);
    }

    // Remove rows for devices no longer present.
    let mut rows = device_rows.borrow_mut();
    rows.retain(|mac, dev_row| {
        let keep = snapshot.states.contains_key(mac);
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

    set(&w.left_level, &w.left_label, state.left_battery,
        &flags(state.left_charging, state.left_in_ear));
    set(&w.right_level, &w.right_label, state.right_battery,
        &flags(state.right_charging, state.right_in_ear));
    set(&w.case_level, &w.case_label, state.case_battery,
        &flags(state.case_charging, false));

    let lid = if state.lid_open { "Open" } else { "Closed" };
    w.status_label
        .set_text(&format!("Model: 0x{:04X} • Lid: {lid}", state.device_model));
}
