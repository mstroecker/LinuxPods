//! System tray indicator via StatusNotifierItem.
//!
//! ksni asks the tray to rebuild its menu from current state, so the menu is a
//! pure function of the struct fields rather than a set of per-item callbacks.

use std::sync::Arc;

use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Handle, MenuItem, Tray, TrayMethods};

use crate::podstate::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseMode {
    Transparency,
    Adaptive,
    NoiseCancelling,
    Off,
}

impl NoiseMode {
    const ALL: [NoiseMode; 4] = [
        Self::Transparency,
        Self::Adaptive,
        Self::NoiseCancelling,
        Self::Off,
    ];

    fn label(&self) -> &'static str {
        match self {
            Self::Transparency => "Transparency",
            Self::Adaptive => "Adaptive",
            Self::NoiseCancelling => "Noise Cancelling",
            Self::Off => "Off",
        }
    }
}

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
    noise_mode: NoiseMode,
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
            noise_mode: NoiseMode::Transparency,
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
        "audio-headphones-symbolic".into()
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
                    checked: self.noise_mode == mode,
                    activate: Box::new(move |this: &mut Self| {
                        this.noise_mode = mode;
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
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_battery_labels() {
        assert_eq!(battery_label("Left", Some(80), false), "  Left : 80%");
        assert_eq!(battery_label("Right", Some(75), true), "  Right: 75% ⚡");
        assert_eq!(battery_label("Case", None, false), "  Case : --");
    }
}
