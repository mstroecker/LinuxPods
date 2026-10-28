//! System tray indicator via StatusNotifierItem.
//!
//! ksni asks the tray to rebuild its menu from current state, so the menu is a
//! pure function of the struct fields rather than a set of per-item callbacks.

use std::sync::Arc;

use bluer::Address;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Handle, MenuItem, Tray, TrayMethods};

use crate::APP_SYMBOLIC_ICON;
use crate::aap::NoiseMode;
use crate::podstate::{DataSource, PodState, Snapshot};

/// Icons ship with the crate rather than only with `make install`, which a
/// `cargo run` never performs. Handing the host this directory is what keeps the
/// tray from falling back to a missing-image glyph in a source checkout.
const ICON_THEME_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons");

/// Actions the tray hands back to the application.
pub trait TrayActions: Send + Sync + 'static {
    fn show_window(&self);
    fn quit(&self);
    fn set_noise_mode(&self, mac: Address, mode: NoiseMode);
}

pub struct Indicator {
    /// The device shown, which is the one noise control commands go to.
    mac: Option<Address>,
    left: Option<u8>,
    right: Option<u8>,
    case: Option<u8>,
    /// What the tooltip shows - see [`PodState::lowest_earbud`].
    lowest_earbud: Option<u8>,
    left_charging: bool,
    right_charging: bool,
    case_charging: bool,
    /// `None` until the connected device reports one: no mode is shown as
    /// checked rather than a guess.
    noise_mode: Option<NoiseMode>,
    /// Switching mode is a command, so the entries are only selectable while an
    /// AAP link is up; over BLE the tray is read-only.
    on_aap: bool,
    actions: Arc<dyn TrayActions>,
}

impl Indicator {
    pub fn new(actions: Arc<dyn TrayActions>) -> Self {
        Self {
            mac: None,
            left: None,
            right: None,
            case: None,
            lowest_earbud: None,
            left_charging: false,
            right_charging: false,
            case_charging: false,
            noise_mode: None,
            on_aap: false,
            actions,
        }
    }

    /// Spawns the tray and returns a handle for later updates.
    ///
    /// The spec has an item own `org.kde.StatusNotifierItem-PID-ID`, which a
    /// Flatpak sandbox cannot grant: it rejects the wildcard such a name needs.
    /// There the item registers under its unique connection name instead.
    pub async fn start(self) -> anyhow::Result<Handle<Indicator>> {
        let sandboxed = std::path::Path::new("/.flatpak-info").exists();
        TrayMethods::disable_dbus_name(self, sandboxed)
            .spawn()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start tray: {e}"))
    }

    /// Shows a device's reading, or with none the state the tray starts in.
    /// Without the reset, a device that has left - its cached reading expired,
    /// its AAP link dropped - would stay on show indefinitely, its noise control
    /// entries still enabled and aimed at it.
    fn show(&mut self, device: Option<(Address, &PodState)>) {
        let Some((mac, state)) = device else {
            *self = Self::new(Arc::clone(&self.actions));
            return;
        };
        self.mac = Some(mac);
        self.left = state.left_battery;
        self.right = state.right_battery;
        self.case = state.case_battery;
        self.lowest_earbud = state.lowest_earbud();
        self.left_charging = state.left_charging;
        self.right_charging = state.right_charging;
        self.case_charging = state.case_charging;
        self.noise_mode = state.noise_mode;
        self.on_aap = state.source == DataSource::Aap;
    }
}

/// "  Left : 80% ⚡", matching the Go layout.
fn battery_label(name: &str, level: Option<u8>, charging: bool) -> String {
    match level {
        Some(v) => {
            let bolt = if charging { " ⚡" } else { "" };
            format!("  {name:<5}: {v}%{bolt}")
        }
        None => format!("  {name:<5}: --"),
    }
}

fn disabled(label: String) -> MenuItem<Indicator> {
    StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

impl Tray for Indicator {
    fn id(&self) -> String {
        "linuxpods".into()
    }

    fn title(&self) -> String {
        "LinuxPods".into()
    }

    fn icon_name(&self) -> String {
        APP_SYMBOLIC_ICON.into()
    }

    /// Empty unless the checkout still exists. The AppIndicator extension looks
    /// the icon up in this path alone when one is given, with no fallback to the
    /// default theme - so a path that is gone (the build dir of a Flatpak, a
    /// moved checkout) hides the installed icon behind a placeholder.
    fn icon_theme_path(&self) -> String {
        if std::path::Path::new(ICON_THEME_PATH).is_dir() {
            ICON_THEME_PATH.into()
        } else {
            String::new()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.lowest_earbud {
            Some(v) => format!("AirPods - {v}%"),
            None => "Searching for AirPods…".to_string(),
        };
        ksni::ToolTip {
            title: "LinuxPods".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            disabled("Battery Levels".into()),
            MenuItem::Separator,
            disabled(battery_label("Left", self.left, self.left_charging)),
            disabled(battery_label("Right", self.right, self.right_charging)),
            disabled(battery_label("Case", self.case, self.case_charging)),
            MenuItem::Separator,
            disabled("Noise Control".into()),
        ];

        for mode in NoiseMode::ALL {
            items.push(
                CheckmarkItem {
                    label: mode.label().into(),
                    checked: self.noise_mode == Some(mode),
                    enabled: self.on_aap,
                    // The checkmark deliberately does not move here. The
                    // coordinator records the mode as soon as the packet is
                    // away and broadcasts, so the menu follows a command that
                    // actually went out - and stays put on one that failed.
                    activate: Box::new(move |this: &mut Self| {
                        if let Some(mac) = this.mac {
                            this.actions.set_noise_mode(mac, mode);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open LinuxPods".into(),
                activate: Box::new(|this: &mut Self| this.actions.show_window()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut Self| this.actions.quit()),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Pushes a new snapshot into the tray. ksni compares the properties and menu
/// before and after, so a snapshot that changes nothing the tray shows sends
/// nothing to the host.
pub async fn apply_snapshot(handle: &Handle<Indicator>, snapshot: &Snapshot) {
    let device = tray_state(snapshot).map(|(mac, state)| (mac, state.clone()));
    handle
        .update(move |tray: &mut Indicator| {
            tray.show(device.as_ref().map(|(mac, state)| (*mac, state)));
        })
        .await;
}

/// The device the tray shows, by the MAC its reading is keyed under, and that
/// reading: the default device, the one the window shows until the user picks
/// another. None while that device has no reading - a link that has not
/// reported yet - rather than another pair's, which would leave the tray and
/// the window on different devices.
fn tray_state(snapshot: &Snapshot) -> Option<(Address, &PodState)> {
    let mac = snapshot.default_device()?;
    snapshot.states.get(&mac).map(|state| (mac, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tray resolves its icon by name under the theme path, and a name that
    /// resolves to nothing shows up as a blank spot in the panel rather than as
    /// an error anyone would see in a log.
    #[test]
    fn the_tray_icon_exists_where_the_host_will_look_for_it() {
        let icon = std::path::Path::new(ICON_THEME_PATH)
            .join("hicolor/symbolic/apps")
            .join(format!("{APP_SYMBOLIC_ICON}.svg"));
        assert!(icon.exists(), "missing tray icon: {}", icon.display());
    }

    /// GTK4 resolves an icon by scanning the tree, so an in-process lookup
    /// succeeds with no index.theme at all. GNOME Shell reads the same tree
    /// through St.IconTheme, forked from GTK3, which enumerates only what
    /// index.theme lists - and the panel shows a placeholder rather than
    /// reporting anything. Installed copies merge with the system hicolor index,
    /// which already lists these two; this file is what covers a source checkout.
    #[test]
    fn the_private_theme_declares_the_directories_the_icons_live_in() {
        let index = std::path::Path::new(ICON_THEME_PATH).join("hicolor/index.theme");
        let text = std::fs::read_to_string(&index)
            .unwrap_or_else(|e| panic!("reading {}: {e}", index.display()));

        for dir in ["symbolic/apps", "scalable/apps"] {
            assert!(
                text.contains(&format!("[{dir}]")),
                "{} has no [{dir}] section",
                index.display()
            );
            assert!(
                text.lines()
                    .find(|l| l.starts_with("Directories="))
                    .is_some_and(|l| l.contains(dir)),
                "{} omits {dir} from Directories=",
                index.display()
            );
        }
    }

    #[test]
    fn formats_battery_labels() {
        assert_eq!(battery_label("Left", Some(80), false), "  Left : 80%");
        assert_eq!(battery_label("Right", Some(75), true), "  Right: 75% ⚡");
        assert_eq!(battery_label("Case", None, false), "  Case : --");
    }

    /// The tray without a host: nothing here activates a menu entry.
    struct NoActions;

    impl TrayActions for NoActions {
        fn show_window(&self) {}
        fn quit(&self) {}
        fn set_noise_mode(&self, _: Address, _: NoiseMode) {}
    }

    const MAC: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

    /// One pair on AAP, charging, in Transparency.
    fn reading() -> Snapshot {
        let state = PodState {
            left_battery: Some(73),
            right_battery: Some(80),
            case_battery: Some(41),
            left_charging: true,
            right_charging: true,
            case_charging: true,
            noise_mode: Some(NoiseMode::Transparency),
            ..PodState::empty(DataSource::Aap)
        };
        Snapshot {
            states: [(MAC, state)].into(),
            connected_macs: vec![MAC],
            ..Default::default()
        }
    }

    #[test]
    fn a_reading_fills_the_tray() {
        let mut tray = Indicator::new(Arc::new(NoActions));
        tray.show(tray_state(&reading()));

        assert_eq!(tray.mac, Some(MAC));
        assert_eq!(
            (tray.left, tray.right, tray.case),
            (Some(73), Some(80), Some(41))
        );
        assert!(tray.left_charging && tray.right_charging && tray.case_charging);
        assert_eq!(tray.noise_mode, Some(NoiseMode::Transparency));
        assert!(tray.on_aap);
        assert_eq!(tray.tool_tip().description, "AirPods - 73%");
    }

    /// An earbud the AirPods report as disconnected is unknown (#108): its line
    /// reads `--`, and the tooltip shows the lowest earbud that is known.
    #[test]
    fn a_disconnected_earbud_shows_as_unknown() {
        let mut snapshot = reading();
        let state = snapshot.states.get_mut(&MAC).unwrap();
        state.left_battery = None;
        state.left_charging = false;
        state.case_battery = None;
        let mut tray = Indicator::new(Arc::new(NoActions));
        tray.show(tray_state(&snapshot));

        assert_eq!((tray.left, tray.right, tray.case), (None, Some(80), None));
        assert_eq!(tray.tool_tip().description, "AirPods - 80%");
        assert_eq!(
            battery_label("Left", tray.left, tray.left_charging),
            "  Left : --"
        );
    }

    /// Records the noise control commands the tray hands back.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(Address, NoiseMode)>>);

    impl TrayActions for Recorder {
        fn show_window(&self) {}
        fn quit(&self) {}
        fn set_noise_mode(&self, mac: Address, mode: NoiseMode) {
            self.0.lock().unwrap().push((mac, mode));
        }
    }

    /// A noise control entry commands the device whose reading the tray shows,
    /// by the MAC the snapshot keys that reading under.
    #[test]
    fn noise_control_commands_the_mac_from_the_snapshot() {
        let recorder = Arc::new(Recorder::default());
        let mut tray = Indicator::new(recorder.clone());
        tray.show(tray_state(&reading()));

        let adaptive = tray
            .menu()
            .into_iter()
            .find_map(|item| match item {
                MenuItem::Checkmark(entry) if entry.label == NoiseMode::Adaptive.label() => {
                    Some(entry)
                }
                _ => None,
            })
            .unwrap();
        (adaptive.activate)(&mut tray);

        assert_eq!(*recorder.0.lock().unwrap(), [(MAC, NoiseMode::Adaptive)]);
    }

    /// Once the last device is gone - its cached reading expired, or its AAP link
    /// dropped with no advertisement cached - the tray is back where it started,
    /// rather than showing the last reading and offering noise control for a
    /// device that is not there.
    #[test]
    fn an_empty_snapshot_resets_the_tray() {
        let mut tray = Indicator::new(Arc::new(NoActions));
        tray.show(tray_state(&reading()));
        tray.show(tray_state(&Snapshot::default()));
        assert_is_reset(&tray);
    }

    /// The default device - the pair on AAP - has not reported yet, while another
    /// pair has a reading. The tray waits for the default device rather than
    /// showing the other pair, which the window, on the default device too, is
    /// not showing.
    #[test]
    fn a_default_device_without_a_reading_resets_the_tray() {
        const OTHER: Address = Address::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        let other = PodState {
            left_battery: Some(50),
            right_battery: Some(50),
            ..PodState::empty(DataSource::Ble)
        };
        let mut snapshot = reading();
        snapshot.states = [(OTHER, other)].into();
        snapshot.known_keys = vec![OTHER];

        let mut tray = Indicator::new(Arc::new(NoActions));
        tray.show(tray_state(&reading()));
        tray.show(tray_state(&snapshot));
        assert_is_reset(&tray);
    }

    /// The tray as it starts: no levels, no device, no noise control entry live.
    fn assert_is_reset(tray: &Indicator) {
        assert_eq!(tray.mac, None);
        assert_eq!((tray.left, tray.right, tray.case), (None, None, None));
        assert!(!tray.left_charging && !tray.right_charging && !tray.case_charging);
        assert_eq!(tray.noise_mode, None);
        assert!(!tray.on_aap);
        assert_eq!(tray.tool_tip().description, "Searching for AirPods…");
        for item in tray.menu() {
            if let MenuItem::Checkmark(entry) = item {
                assert!(
                    !entry.enabled && !entry.checked,
                    "{} is still live",
                    entry.label
                );
            }
        }
    }
}
