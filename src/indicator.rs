//! System tray indicator via StatusNotifierItem.
//!
//! ksni asks the tray to rebuild its menu from current state, so the menu is a
//! pure function of the struct fields rather than a set of per-item callbacks.

use std::sync::Arc;

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Handle, MenuItem, Tray, TrayMethods};

use crate::aap::NoiseMode;
use crate::podstate::{DataSource, Snapshot};

/// Icons ship with the crate rather than only with `make install`, which a
/// `cargo run` never performs. Handing the host this directory is what keeps the
/// tray from falling back to a missing-image glyph in a source checkout.
const ICON_THEME_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons");

/// Symbolic, so the panel recolours it to match its own foreground.
const ICON_NAME: &str = "com.linuxpods.app-symbolic";

/// Actions the tray hands back to the application.
pub trait TrayActions: Send + Sync + 'static {
    fn show_window(&self);
    fn quit(&self);
    fn set_noise_mode(&self, mode: NoiseMode);
}

pub struct Indicator {
    left: Option<u8>,
    right: Option<u8>,
    case: Option<u8>,
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
            left: None,
            right: None,
            case: None,
            left_charging: false,
            right_charging: false,
            case_charging: false,
            noise_mode: None,
            on_aap: false,
            actions,
        }
    }

    /// Spawns the tray and returns a handle for later updates.
    pub async fn start(self) -> anyhow::Result<Handle<Indicator>> {
        TrayMethods::spawn(self)
            .await
            .map_err(|e| anyhow::anyhow!("failed to start tray: {e}"))
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
        ICON_NAME.into()
    }

    fn icon_theme_path(&self) -> String {
        ICON_THEME_PATH.into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let lowest = match (self.left, self.right) {
            (Some(l), Some(r)) => Some(l.min(r)),
            (Some(v), None) | (None, Some(v)) => Some(v),
            (None, None) => None,
        };
        let description = match lowest {
            Some(v) => format!("AirPods - {v}%"),
            None => "Searching for AirPods...".to_string(),
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
                        this.actions.set_noise_mode(mode);
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

/// Pushes a new snapshot into the tray.
pub async fn apply_snapshot(handle: &Handle<Indicator>, snapshot: &Snapshot) {
    let Some(state) = snapshot.primary().cloned() else {
        return;
    };
    handle
        .update(move |tray: &mut Indicator| {
            tray.left = state.left_battery;
            tray.right = state.right_battery;
            tray.case = state.case_battery;
            tray.left_charging = state.left_charging;
            tray.right_charging = state.right_charging;
            tray.case_charging = state.case_charging;
            tray.noise_mode = state.noise_mode;
            tray.on_aap = state.source == DataSource::Aap;
        })
        .await;
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
            .join(format!("{ICON_NAME}.svg"));
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
}
