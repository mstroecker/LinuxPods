//! GTK4/libadwaita interface.
//!
//! Updates arrive from the coordinator over an async channel consumed on the GTK
//! main context. That is the only path into the UI: GTK types are !Send, so a
//! background task can never touch a widget directly.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::aap::NoiseMode;
use crate::ble::decode_connection_state;
use crate::podstate::{Coordinator, DataSource, PodState, Snapshot};

/// One column of the battery display: a pod, or the case.
pub struct BatteryColumn {
    pub level: gtk::LevelBar,
    pub label: gtk::Label,
    /// Shown while charging, drawn at the current level.
    pub charging: gtk::Image,
    /// Shown while the pod is in an ear; the case never reports it.
    pub in_ear: gtk::Image,
}

impl BatteryColumn {
    /// Shows a level, or `--` when unknown, and whichever icons apply.
    fn show(&self, level: Option<u8>, charging: bool, in_ear: bool) {
        match level {
            Some(v) => {
                self.level.set_value(f64::from(v) / 100.0);
                self.label.set_text(&format!("{v}%"));
            }
            None => {
                self.level.set_value(0.0);
                self.label.set_text("--");
            }
        }
        if charging {
            self.charging.set_icon_name(Some(&charging_icon(level)));
        }
        self.charging.set_visible(charging);
        self.in_ear.set_visible(in_ear);
    }
}

pub struct BatteryWidgets {
    pub left: BatteryColumn,
    pub right: BatteryColumn,
    pub case: BatteryColumn,
    pub status_label: gtk::Label,
    /// How old a BLE reading is. Hidden for AAP, which is always live.
    pub last_seen_label: gtk::Label,
}

/// The charging icon for a level, as GNOME Shell draws a charging battery: the
/// level in steps of ten, with a bolt. The theme has no bare bolt, and a fixed
/// full battery would contradict the percentage beside it.
fn charging_icon(level: Option<u8>) -> String {
    match level.map(|v| (v.min(100) + 5) / 10 * 10) {
        Some(100) => "battery-level-100-charged-symbolic".to_string(),
        Some(step) => format!("battery-level-{step}-charging-symbolic"),
        // Charging with no level to show: a generic charging battery.
        None => "battery-full-charging-symbolic".to_string(),
    }
}

/// Anything heard within this long counts as advertising now.
const RECENT: Duration = Duration::from_secs(60);

/// Icon name of the app icon, installed into hicolor by `make install`.
const APP_ICON: &str = "com.linuxpods.app";

/// Drawn for this app and bundled: the theme has no icon for an earbud being
/// worn, and headphones read as "audio device" rather than "in an ear".
const IN_EAR_ICON: &str = "linuxpods-in-ear-symbolic";

const WEBSITE: &str = "https://github.com/mstroecker/LinuxPods";

/// Window action for noise control. Its state names the mode shown, by
/// `noise_target`, and is `""` while no mode has been reported.
const NOISE_ACTION: &str = "noise-mode";

/// Prefix of the resources `build.rs` compiles from `assets/`.
const RESOURCE_PREFIX: &str = "/com/linuxpods/app";

/// Left pod, right pod, case - the order of the battery display's columns.
const BATTERY_IMAGES: [&str; 3] = [
    "left_airpod_pro3.png",
    "right_airpod_pro3.png",
    "airpod_case.png",
];

/// Registers the resources compiled into the binary. Call once, before the
/// window is built.
pub fn register_resources() {
    gio::resources_register_include!("linuxpods.gresource")
        .expect("the bundled GResource is valid");
}

/// Builds the window and wires it to the coordinator. The caller presents it,
/// so that `--minimized` can leave it built but hidden.
pub fn activate(
    app: &adw::Application,
    coordinator: std::sync::Arc<Coordinator>,
    runtime: tokio::runtime::Handle,
) -> adw::ApplicationWindow {
    let updates = coordinator.subscribe();
    let win = adw::ApplicationWindow::new(app);
    win.set_title(Some("LinuxPods"));
    // Tall enough for the whole Control page without scrolling.
    win.set_default_size(420, 680);

    // Closing hides the window instead of destroying it. It is the app's only
    // window, so destroying it quit the whole process, the tray and the GNOME
    // Settings battery with it. Hidden, it is exactly the `--minimized` state; the
    // tray and the launcher bring it back.
    win.set_hide_on_close(true);

    let (control, prefs, dev_group) = setup_ui(&win);
    let control = Rc::new(control);

    // The primary menu's entries. Preferences is built once and kept, since the
    // update loop fills its Development group whether it is open or not.
    win.add_action_entries([
        gio::ActionEntry::builder("preferences")
            .activate(move |win: &adw::ApplicationWindow, _, _| prefs.present(Some(win)))
            .build(),
        gio::ActionEntry::builder("about")
            .activate(|win: &adw::ApplicationWindow, _, _| show_about(win))
            .build(),
    ]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);

    // Switching mode is a command: it goes out over the tokio runtime, and the
    // action's state follows the coordinator's snapshot rather than the click. A
    // failed command therefore leaves the previous mode selected instead of
    // claiming one the AirPods never switched to.
    //
    // Handling `activate` keeps GIO from changing the state on its own, and a
    // state set from a snapshot is never an activation, so nothing needs guarding
    // against a snapshot echoing back out as a command.
    let coord = coordinator.clone();
    let rt = runtime.clone();
    control.noise_action.connect_activate(move |_, target| {
        let Some(mode) = target
            .and_then(|t| t.str())
            .and_then(noise_mode_from_target)
        else {
            return;
        };
        let coord = coord.clone();
        rt.spawn(async move {
            if let Err(e) = coord.set_noise_control(mode).await {
                tracing::warn!("failed to set noise control: {e:#}");
            }
        });
    });
    win.add_action(&control.noise_action);

    // Updates arrive over an async channel consumed on the main context. GTK types
    // are !Send, so this is the only legal way in - enforced at compile time.
    let device_rows: Rc<RefCell<HashMap<String, DeviceRow>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Which device the Control page is showing, and the snapshot behind it, so a
    // switcher change can redraw without waiting for the next update.
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let last_snapshot: Rc<RefCell<Option<Snapshot>>> = Rc::new(RefCell::new(None));
    // Guards against the programmatic set_selected() below re-entering this handler.
    let syncing = Rc::new(Cell::new(false));

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

    // A cached reading sits unchanged, so nothing is broadcast for it and its
    // "Last seen" would freeze. Redraw from the last snapshot to advance it.
    glib::timeout_add_seconds_local(
        30,
        glib::clone!(
            #[strong]
            control,
            #[strong]
            selected,
            #[strong]
            last_snapshot,
            #[strong]
            dev_group,
            #[strong]
            device_rows,
            #[strong]
            coordinator,
            #[strong]
            runtime,
            move || {
                if let Some(snapshot) = last_snapshot.borrow().as_ref() {
                    let mac = selected.borrow().clone();
                    render_device(&control, snapshot, mac.as_deref());
                    update_device_rows(&dev_group, &device_rows, snapshot, &coordinator, &runtime);
                }
                glib::ControlFlow::Continue
            }
        ),
    );

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

/// The window holds the Control page alone. Preferences and About open from the
/// primary menu, as dialogs, rather than sitting beside it as tabs.
///
/// Returns the preferences dialog, and its Development group for the update loop
/// to populate.
fn setup_ui(
    win: &adw::ApplicationWindow,
) -> (ControlView, adw::PreferencesDialog, adw::PreferencesGroup) {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("win.preferences"));
    menu.append(Some("_About LinuxPods"), Some("win.about"));
    // Closing the window only hides it, so quitting needs a place of its own -
    // not least without a tray, where this is the only way out.
    let quit_section = gio::Menu::new();
    quit_section.append(Some("_Quit"), Some("app.quit"));
    menu.append_section(None, &quit_section);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build();

    let (control_page, control) = create_control_view();
    let (prefs, dev_group) = create_preferences_dialog();

    // The device switcher takes the title's place when there is a choice to
    // make, the way a view switcher would; with a single device the header shows
    // the app name as usual.
    let window_title = adw::WindowTitle::new("LinuxPods", "");
    control
        .device_dropdown
        .bind_property("visible", &window_title, "visible")
        .invert_boolean()
        .sync_create()
        .build();
    let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    title_box.append(&window_title);
    title_box.append(&control.device_dropdown);

    let header_bar = adw::HeaderBar::new();
    header_bar.set_title_widget(Some(&title_box));
    header_bar.pack_end(&menu_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&control_page));

    win.set_content(Some(&toolbar_view));

    (control, prefs, dev_group)
}

/// Built on demand: nothing in it changes while it is open.
fn show_about(win: &adw::ApplicationWindow) {
    adw::AboutDialog::builder()
        .application_name("LinuxPods")
        .application_icon(APP_ICON)
        .comments("Manage Apple AirPods on Linux")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Marcel Ströcker")
        .website(WEBSITE)
        .issue_url(format!("{WEBSITE}/issues"))
        .license_type(gtk::License::Gpl30)
        .build()
        .present(Some(win));
}

/// Everything on the Control page the update loop needs to touch.
pub struct ControlView {
    pub battery: BatteryWidgets,
    /// The device switcher, in the header bar in place of the title; hidden
    /// unless more than one device is identified.
    pub device_dropdown: gtk::DropDown,
    pub device_list: gtk::StringList,
    /// MAC per row in `device_list`, kept parallel so the display string can differ.
    pub device_macs: RefCell<Vec<String>>,
    /// The labels currently in the model. Kept alongside the MACs so a rename or a
    /// newly decoded model name refreshes the switcher even though the MAC list is
    /// unchanged.
    pub device_labels: RefCell<Vec<String>>,
    /// `win.noise-mode`, enabled only while the device shown is on AAP. The Noise
    /// Control group's sensitivity is bound to it.
    pub noise_action: gio::SimpleAction,
    pub features_group: adw::PreferencesGroup,
}

/// The Control page. An `AdwPreferencesPage` for its scrolling, width clamp and
/// group spacing, although only the lower half is a list of settings.
fn create_control_view() -> (adw::PreferencesPage, ControlView) {
    let page = adw::PreferencesPage::new();

    // Battery display and status line. Not rows, so they share a plain box in an
    // untitled group rather than a boxed list.
    let control_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .build();
    let status_group = adw::PreferencesGroup::new();
    status_group.add(&control_box);
    page.add(&status_group);

    // Device switcher, placed in the header bar by `setup_ui`. Only shown when
    // more than one device is identified, so the common single-device case keeps
    // the plain title. It scopes the whole page, and the header is where GNOME
    // apps put that kind of selector. What it selects is evident from where it
    // sits, so the explanation lives in a tooltip.
    let device_list = gtk::StringList::new(&[]);
    let device_dropdown = gtk::DropDown::builder()
        .model(&device_list)
        .tooltip_text("Which AirPods these readings are from")
        .visible(false)
        .build();
    // Flat, so it reads as the title until hovered. A dropdown in a header bar
    // stays raised, and `flat` on the dropdown itself matches nothing in
    // Adwaita's stylesheet: the rules style the button inside it, the
    // `dropdown > button` node GTK documents.
    if let Some(button) = device_dropdown.first_child() {
        button.add_css_class("flat");
    }

    let battery_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(20)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .build();

    let [left, right, case] = BATTERY_IMAGES.map(|name| {
        let column_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .build();

        let image = gtk::Image::from_resource(&format!("{RESOURCE_PREFIX}/{name}"));
        image.set_pixel_size(64);
        column_box.append(&image);

        let level = gtk::LevelBar::new();
        level.set_mode(gtk::LevelBarMode::Continuous);
        level.set_value(0.0);
        level.set_size_request(100, 20);
        column_box.append(&level);

        // The percentage, then what the pod is doing. Symbolic icons rather than
        // emoji, so they take the theme's colour and dim along with the text.
        let label = gtk::Label::new(Some("--"));
        let charging = gtk::Image::builder()
            .tooltip_text("Charging")
            .visible(false)
            .build();
        let in_ear = gtk::Image::builder()
            .icon_name(IN_EAR_ICON)
            .tooltip_text("In ear")
            .visible(false)
            .build();
        let reading = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .build();
        reading.add_css_class("dim-label");
        reading.append(&label);
        reading.append(&charging);
        reading.append(&in_ear);
        column_box.append(&reading);

        battery_box.append(&column_box);
        BatteryColumn {
            level,
            label,
            charging,
            in_ear,
        }
    });

    control_box.append(&battery_box);

    let status_label = gtk::Label::new(Some("Searching for AirPods..."));
    status_label.add_css_class("dim-label");
    status_label.set_margin_top(10);
    control_box.append(&status_label);

    let last_seen_label = gtk::Label::builder().visible(false).build();
    last_seen_label.add_css_class("dim-label");
    last_seen_label.add_css_class("caption");
    control_box.append(&last_seen_label);

    let widgets = BatteryWidgets {
        left,
        right,
        case,
        status_label,
        last_seen_label,
    };

    // Noise Control
    let noise_control_group = adw::PreferencesGroup::builder()
        .title("Noise Control")
        .build();

    // The empty state selects nothing: until the device reports its mode, showing
    // a row as chosen would claim a mode the AirPods may well not be in. The
    // handler is attached in `activate`, which is where the coordinator to send
    // the command to lives.
    let noise_action = gio::SimpleAction::new_stateful(
        NOISE_ACTION,
        Some(glib::VariantTy::STRING),
        &"".to_variant(),
    );
    noise_action.set_enabled(false);
    // Dims the rows along with the radio buttons.
    noise_action
        .bind_property("enabled", &noise_control_group, "sensitive")
        .sync_create()
        .build();

    // The modes, their labels and their order all come from the protocol layer,
    // so this list cannot drift from the tray's.
    for mode in NoiseMode::ALL {
        let row = adw::ActionRow::builder()
            .title(mode.label())
            .subtitle(mode.description())
            .build();

        // With an action and a target, a check button draws as a radio and is
        // active exactly when the action's state equals its target - which is
        // what groups the four, so no set_group is needed.
        let radio_button = gtk::CheckButton::new();
        radio_button
            .set_detailed_action_name(&format!("win.{NOISE_ACTION}::{}", noise_target(mode)));

        row.add_prefix(&radio_button);
        row.set_activatable_widget(Some(&radio_button));
        noise_control_group.add(&row);
    }
    page.add(&noise_control_group);

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
    page.add(&conversation_group);

    let view = ControlView {
        battery: widgets,
        device_dropdown,
        device_list,
        device_macs: RefCell::new(Vec::new()),
        device_labels: RefCell::new(Vec::new()),
        noise_action,
        features_group: conversation_group,
    };

    (page, view)
}

/// The action target naming a mode. Stable identifiers, not labels, so the
/// wording can change without touching the bindings.
fn noise_target(mode: NoiseMode) -> &'static str {
    match mode {
        NoiseMode::Off => "off",
        NoiseMode::NoiseCancelling => "noise-cancelling",
        NoiseMode::Transparency => "transparency",
        NoiseMode::Adaptive => "adaptive",
    }
}

fn noise_mode_from_target(target: &str) -> Option<NoiseMode> {
    NoiseMode::ALL
        .into_iter()
        .find(|m| noise_target(*m) == target)
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
            view.noise_action.set_enabled(interactive);
            view.features_group.set_sensitive(interactive);
            show_noise_mode(view, state.noise_mode);
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
            view.noise_action.set_enabled(false);
            view.features_group.set_sensitive(false);
            show_noise_mode(view, None);
        }
    }
}

/// Selects the row for `mode`, or clears the group when nothing has reported one.
/// Setting the state is not an activation, so nothing goes out to the device.
fn show_noise_mode(view: &ControlView, mode: Option<NoiseMode>) {
    let target = mode.map_or("", noise_target);
    view.noise_action.set_state(&target.to_variant());
}

/// Resets the display, with the reason shown in the status line.
fn clear_battery_display(w: &BatteryWidgets, status: &str) {
    for column in [&w.left, &w.right, &w.case] {
        column.show(None, false, false);
    }
    w.status_label.set_text(status);
    w.last_seen_label.set_visible(false);
}

/// The Preferences dialog. Returns the Development group so the update loop can
/// populate it.
fn create_preferences_dialog() -> (adw::PreferencesDialog, adw::PreferencesGroup) {
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
    (dialog, dev_group)
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

        let (text, css) = match (known, connected, state) {
            (_, true, _) => ("Connected".to_string(), "success"),
            // Advertising but no stored key: visible, not yet decryptable. Its MAC
            // rotates, so these entries come and go until a key is captured.
            (false, _, _) => ("No key".to_string(), "warning"),
            // A cached reading is not an advertisement in progress.
            (true, _, Some(s)) => match s.last_seen.map(|t| t.elapsed()) {
                Some(age) if age >= RECENT => (last_seen_text(age), "dim-label"),
                _ => ("Advertising".to_string(), "dim-label"),
            },
            (true, _, None) => ("Idle".to_string(), "dim-label"),
        };
        dev_row.status_label.set_text(&text);
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

/// Draws a reading into the battery display and the status line.
fn update_battery_display(w: &BatteryWidgets, state: &PodState) {
    w.left
        .show(state.left_battery, state.left_charging, state.left_in_ear);
    w.right.show(
        state.right_battery,
        state.right_charging,
        state.right_in_ear,
    );
    w.case.show(state.case_battery, state.case_charging, false);

    w.status_label.set_text(&status_line(state));

    match state.last_seen {
        Some(seen) => {
            w.last_seen_label.set_text(&last_seen_text(seen.elapsed()));
            w.last_seen_label.set_visible(true);
        }
        None => w.last_seen_label.set_visible(false),
    }
}

/// How long ago a BLE reading was heard. Whole minutes are plenty: the cache
/// holds readings for half an hour, and the display is redrawn every 30 seconds.
fn last_seen_text(age: Duration) -> String {
    if age < RECENT {
        return "Last seen just now".to_string();
    }
    format!("Last seen {} min ago", age.as_secs() / 60)
}

/// The one-line summary under the battery display.
fn status_line(state: &PodState) -> String {
    // Absent unless the earbuds are in the case; the case is what reports it.
    let lid = match state.lid_open {
        Some(true) => " • Lid: Open",
        Some(false) => " • Lid: Closed",
        None => "",
    };
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

    format!("{model}{lid}{activity} • Source: {source}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing resource fails silently - gtk::Image::from_resource renders
    /// nothing, and the About dialog shows a placeholder for the app icon - so
    /// every name the UI loads must be in the bundle.
    #[test]
    fn bundled_resources_resolve() {
        register_resources();
        let icons = [
            format!("icons/scalable/apps/{APP_ICON}.svg"),
            format!("icons/scalable/status/{IN_EAR_ICON}.svg"),
        ];
        for name in BATTERY_IMAGES
            .into_iter()
            .chain(icons.iter().map(String::as_str))
        {
            let path = format!("{RESOURCE_PREFIX}/{name}");
            assert!(
                gio::resources_get_info(&path, gio::ResourceLookupFlags::NONE).is_ok(),
                "missing resource: {path}"
            );
        }
    }

    #[test]
    fn status_line_shows_what_the_airpods_are_doing() {
        let state = PodState {
            source: DataSource::Ble,
            model_name: "AirPods Pro 3".into(),
            lid_open: Some(true),
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
        assert_eq!(status_line(&state), "AirPods Pro 3 • Source: AAP");
    }

    #[test]
    fn charging_icon_follows_the_level_in_steps_of_ten() {
        assert_eq!(charging_icon(Some(4)), "battery-level-0-charging-symbolic");
        assert_eq!(
            charging_icon(Some(26)),
            "battery-level-30-charging-symbolic"
        );
        assert_eq!(
            charging_icon(Some(94)),
            "battery-level-90-charging-symbolic"
        );
        // Rounds up to full, which the theme draws as charged.
        assert_eq!(
            charging_icon(Some(95)),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(
            charging_icon(Some(100)),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(charging_icon(None), "battery-full-charging-symbolic");
    }

    /// Every mode survives the trip through its action target, and the empty
    /// state - no mode reported yet - selects none of them.
    #[test]
    fn noise_targets_round_trip() {
        for mode in NoiseMode::ALL {
            assert_eq!(noise_mode_from_target(noise_target(mode)), Some(mode));
        }
        assert_eq!(noise_mode_from_target(""), None);
    }

    #[test]
    fn last_seen_counts_whole_minutes() {
        assert_eq!(last_seen_text(Duration::from_secs(5)), "Last seen just now");
        assert_eq!(
            last_seen_text(Duration::from_secs(60)),
            "Last seen 1 min ago"
        );
        assert_eq!(
            last_seen_text(Duration::from_secs(29 * 60 + 59)),
            "Last seen 29 min ago"
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
