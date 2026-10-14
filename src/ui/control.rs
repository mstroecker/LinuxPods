//! The Control page: battery display, status line, device switcher, Noise
//! Control and Features.

use std::cell::RefCell;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{RESOURCE_PREFIX, last_seen_text};
use crate::aap::NoiseMode;
use crate::ble::decode_connection_state;
use crate::podstate::{DataSource, PodState, Snapshot};

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

/// Drawn for this app and bundled: the theme has no icon for an earbud being
/// worn, and headphones read as "audio device" rather than "in an ear".
pub(super) const IN_EAR_ICON: &str = "linuxpods-in-ear-symbolic";

/// The app icon as a symbolic, for the empty state. Bundled, so it resolves
/// without an install.
pub(super) const APP_SYMBOLIC_ICON: &str = "io.github.mstroecker.LinuxPods-symbolic";

/// Window action for noise control. Its state names the mode shown, by
/// `noise_target`, and is `""` while no mode has been reported.
const NOISE_ACTION: &str = "noise-mode";

/// Left pod, right pod, case - the order of the battery display's columns.
pub(super) const BATTERY_IMAGES: [&str; 3] = [
    "left_airpod_pro3.png",
    "right_airpod_pro3.png",
    "airpod_case.png",
];

/// Everything on the Control page the update loop needs to touch.
pub struct ControlView {
    /// The window's content: the Control page as `device`, or the empty state
    /// as `empty` while no device has a stored key.
    pub content: gtk::Stack,
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
pub(super) fn create_control_view() -> ControlView {
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

    let status_label = gtk::Label::new(Some("Searching for AirPods…"));
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

    let content = gtk::Stack::new();
    content.add_named(&page, Some("device"));
    content.add_named(&create_empty_state(), Some("empty"));

    ControlView {
        content,
        battery: widgets,
        device_dropdown,
        device_list,
        device_macs: RefCell::new(Vec::new()),
        device_labels: RefCell::new(Vec::new()),
        noise_action,
        features_group: conversation_group,
    }
}

/// Shown instead of the Control page until a device has a stored key. The page
/// and the switcher follow the keystore, and a key arrives only over AAP - it is
/// requested on every connection - so until a pair has connected once there is
/// nothing to show. Connecting is the whole setup, so there is no button.
fn create_empty_state() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(APP_SYMBOLIC_ICON)
        .title("Connect Your AirPods")
        .description(
            "Connect your AirPods to this computer over Bluetooth, and they will show up here",
        )
        .build()
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

pub(super) fn noise_mode_from_target(target: &str) -> Option<NoiseMode> {
    NoiseMode::ALL
        .into_iter()
        .find(|m| noise_target(*m) == target)
}

/// Repopulates the switcher, preserving the current selection where possible.
pub(super) fn sync_device_list(view: &ControlView, macs: &[String], snapshot: &Snapshot) {
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
pub(super) fn render_device(view: &ControlView, snapshot: &Snapshot, mac: Option<&str>) {
    let state = mac.and_then(|m| snapshot.states.get(m));
    let connected = mac.is_some_and(|m| snapshot.is_connected(m));

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
