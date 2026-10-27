//! GTK4/libadwaita interface.
//!
//! Updates arrive from the coordinator over a watch channel consumed on the GTK
//! main context. That is the only path into the UI's state: GTK types are
//! !Send, so a background task can never touch a widget directly. Commands go
//! out with `Coordinator::command`, and each one's result comes back to the main
//! context too, where a failure goes to a toast.
//!
//! The controls, and the labels a test reads, carry an ID set with
//! `set_widget_name`, for the test harness (#74) to find them by; `automation`
//! lists them. Every build sets them: they cost nothing, and a harness build
//! then lays out the same widgets as the one users run. Under the harness,
//! `automation` presses widgets over D-Bus as a user would - from GDBus
//! handlers on the main context, never from a background task.

use std::time::Duration;

use gtk::gio;

#[cfg(feature = "harness")]
mod automation;
mod control;
mod preferences;
mod window;

#[cfg(feature = "harness")]
pub use automation::register as register_automation;
pub use window::activate;

/// Anything heard within this long counts as advertising now.
const RECENT: Duration = Duration::from_secs(60);

/// Prefix of the resources `build.rs` compiles from `assets/`.
const RESOURCE_PREFIX: &str = "/io/github/mstroecker/LinuxPods";

/// Registers the resources compiled into the binary. Call once, before the
/// window is built.
pub fn register_resources() {
    gio::resources_register_include!("linuxpods.gresource")
        .expect("the bundled GResource is valid");
}

/// How long ago a BLE reading was heard. Whole minutes are plenty: the cache
/// holds readings for half an hour, and the display is redrawn every 30 seconds.
fn last_seen_text(age: Duration) -> String {
    if age < RECENT {
        return "Last seen just now".to_string();
    }
    format!("Last seen {} min ago", age.as_secs() / 60)
}

#[cfg(test)]
mod tests {
    use super::control::{APP_SYMBOLIC_ICON, BATTERY_IMAGES, IN_EAR_ICON};
    use super::window::APP_ICON;
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
            format!("icons/scalable/apps/{APP_SYMBOLIC_ICON}.svg"),
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
}
