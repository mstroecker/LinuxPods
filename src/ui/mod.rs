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

use crate::podstate::{Coordinator, Snapshot};

mod control;
mod preferences;

use control::{
    ControlView, create_control_view, noise_mode_from_target, render_device, sync_device_list,
};
use preferences::{DeviceRow, create_preferences_dialog, update_device_rows};

/// Anything heard within this long counts as advertising now.
const RECENT: Duration = Duration::from_secs(60);

/// Shortest gap between two renders. Readings change over minutes, while a
/// replayed advertisement can arrive every few milliseconds.
const RENDER_INTERVAL: Duration = Duration::from_millis(100);

/// Icon name of the app icon, installed into hicolor by `make install`.
const APP_ICON: &str = "io.github.mstroecker.LinuxPods";

const WEBSITE: &str = "https://github.com/mstroecker/LinuxPods";

/// Prefix of the resources `build.rs` compiles from `assets/`.
const RESOURCE_PREFIX: &str = "/io/github/mstroecker/LinuxPods";

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
    let mut updates = coordinator.subscribe();
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

    // Updates arrive over a watch channel consumed on the main context. GTK types
    // are !Send, so this is the only legal way in - enforced at compile time.
    let device_rows: Rc<RefCell<HashMap<String, DeviceRow>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Which device the Control page is showing, and the snapshot behind it, so a
    // switcher change can redraw without waiting for the next update.
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let last_snapshot: Rc<RefCell<Option<Snapshot>>> = Rc::new(RefCell::new(None));
    // Guards against the programmatic set_selected() below re-entering this handler.
    let syncing = Rc::new(Cell::new(false));

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
    let noise_target_device = selected.clone();
    control.noise_action.connect_activate(move |_, target| {
        let Some(mode) = target
            .and_then(|t| t.str())
            .and_then(noise_mode_from_target)
        else {
            return;
        };
        // The device on show: with several pairs connected, each has its own link.
        let Some(mac) = noise_target_device.borrow().clone() else {
            return;
        };
        let coord = coord.clone();
        rt.spawn(async move {
            if let Err(e) = coord.set_noise_control(&mac, mode).await {
                tracing::warn!("failed to set noise control: {e:#}");
            }
        });
    });
    win.add_action(&control.noise_action);

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
        while updates.changed().await.is_ok() {
            // Cloned out: the borrow is a read lock every broadcast waits for, so it
            // must not live across an await.
            let snapshot = updates.borrow_and_update().clone();

            // The switcher lists every device we hold a key for. That keeps the list
            // stable instead of flickering as advertisements arrive, and strangers
            // cannot appear: an advertisement we could not decrypt is never stored.
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
                .or_else(|| {
                    snapshot
                        .connected_macs
                        .iter()
                        .find(|m| macs.contains(m))
                        .cloned()
                })
                .or_else(|| macs.first().cloned());
            *selected.borrow_mut() = chosen.clone();

            if let Some(mac) = &chosen
                && let Some(idx) = macs.iter().position(|m| m == mac)
            {
                control.device_dropdown.set_selected(idx as u32);
            }
            syncing.set(false);

            // With no stored key there is no device to show; the empty state says
            // how to get one.
            let page = if macs.is_empty() { "empty" } else { "device" };
            control.content.set_visible_child_name(page);

            render_device(&control, &snapshot, chosen.as_deref());
            update_device_rows(&dev_group, &device_rows, &snapshot, &coordinator, &runtime);
            *last_snapshot.borrow_mut() = Some(snapshot);

            // tokio's cooperative budget does not reach the GTK main context, so
            // under a stream of updates `changed()` would stay ready and this loop
            // would never yield to GTK. The pause caps rendering, not delivery: the
            // next pass picks up whatever is newest.
            glib::timeout_future(RENDER_INTERVAL).await;
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

    let control = create_control_view();
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
    toolbar_view.set_content(Some(&control.content));

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
