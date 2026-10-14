//! The main window: header bar, primary menu and About dialog, and the update
//! loop that draws the coordinator's snapshots into them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::control::{
    ControlView, choose_device, create_control_view, noise_mode_from_target, render_device,
    sync_device_list,
};
use super::preferences::{DeviceRows, create_preferences_dialog};
use crate::podstate::{Coordinator, Snapshot};

/// Shortest gap between two renders. Readings change over minutes, while a
/// replayed advertisement can arrive every few milliseconds.
const RENDER_INTERVAL: Duration = Duration::from_millis(100);

/// Icon name of the app icon, installed into hicolor by `make install`.
pub(super) const APP_ICON: &str = "io.github.mstroecker.LinuxPods";

const WEBSITE: &str = "https://github.com/mstroecker/LinuxPods";

/// Builds the window and wires it to the coordinator. The caller presents it,
/// so that `--minimized` can leave it built but hidden.
pub fn activate(
    app: &adw::Application,
    coordinator: Arc<Coordinator>,
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

    let control = create_control_view();
    let (prefs, devices) = create_preferences_dialog(coordinator.clone(), runtime.clone());
    setup_ui(&win, &control);

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

    let ui = Rc::new(Ui {
        control,
        devices,
        selected: RefCell::default(),
        last_snapshot: RefCell::default(),
        syncing: Cell::default(),
    });

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
    ui.control.noise_action.connect_activate(glib::clone!(
        #[strong]
        ui,
        move |_, target| {
            let Some(mode) = target
                .and_then(|t| t.str())
                .and_then(noise_mode_from_target)
            else {
                return;
            };
            // The device on show: with several pairs connected, each has its own link.
            let Some(mac) = ui.selected.borrow().clone() else {
                return;
            };
            let coord = coord.clone();
            rt.spawn(async move {
                if let Err(e) = coord.set_noise_control(&mac, mode).await {
                    tracing::warn!("failed to set noise control: {e:#}");
                }
            });
        }
    ));
    win.add_action(&ui.control.noise_action);

    ui.control
        .device_dropdown
        .connect_selected_notify(glib::clone!(
            #[strong]
            ui,
            move |dropdown| ui.select(dropdown.selected())
        ));

    // A cached reading sits unchanged, so nothing is broadcast for it and its
    // "Last seen" would freeze. Redraw from the last snapshot to advance it.
    glib::timeout_add_seconds_local(
        30,
        glib::clone!(
            #[strong]
            ui,
            move || {
                ui.redraw();
                glib::ControlFlow::Continue
            }
        ),
    );

    // Updates arrive over a watch channel consumed on the main context. GTK types
    // are !Send, so this is the only legal way in - enforced at compile time.
    glib::spawn_future_local(async move {
        while updates.changed().await.is_ok() {
            // Cloned out: the borrow is a read lock every broadcast waits for, so it
            // must not live across an await.
            let snapshot = updates.borrow_and_update().clone();
            ui.apply(snapshot);

            // tokio's cooperative budget does not reach the GTK main context, so
            // under a stream of updates `changed()` would stay ready and this loop
            // would never yield to GTK. The pause caps rendering, not delivery: the
            // next pass picks up whatever is newest.
            glib::timeout_future(RENDER_INTERVAL).await;
        }
    });

    win
}

/// The widgets that change once the window is built, and the state that the
/// update loop, the switcher and the clock share to redraw them. Lives in an
/// `Rc` on the main context: GTK types are !Send.
struct Ui {
    control: ControlView,
    /// The Development group in Preferences.
    devices: DeviceRows,
    /// Which device the Control page is showing.
    selected: RefCell<Option<String>>,
    /// The snapshot behind the display, so a switcher change or the clock can
    /// redraw without waiting for the next update.
    last_snapshot: RefCell<Option<Snapshot>>,
    /// Set while `apply` updates the switcher, whose `selected-notify` would
    /// otherwise reach `select` and clobber the user's choice.
    syncing: Cell<bool>,
}

impl Ui {
    /// Takes in a snapshot from the coordinator.
    fn apply(&self, snapshot: Snapshot) {
        // The switcher lists every device we hold a key for. That keeps the list
        // stable instead of flickering as advertisements arrive, and strangers
        // cannot appear: an advertisement we could not decrypt is never stored.
        let macs = &snapshot.known_keys;

        // Read the selection BEFORE touching the widget. Splicing the model makes
        // the dropdown emit selected-notify, and if that ran unguarded it would clobber
        // the user's choice with index 0 - which is why switching device appeared
        // to snap back to the AAP-connected one.
        let current = self.selected.borrow().clone();

        self.syncing.set(true);
        sync_device_list(&self.control, macs, &snapshot);
        let chosen = choose_device(current, macs, &snapshot.connected_macs);
        let position = chosen
            .as_ref()
            .and_then(|mac| macs.iter().position(|m| m == mac));
        *self.selected.borrow_mut() = chosen;
        if let Some(position) = position {
            self.control.device_dropdown.set_selected(position as u32);
        }
        self.syncing.set(false);

        // With no stored key there is no device to show; the empty state says
        // how to get one.
        let page = if macs.is_empty() { "empty" } else { "device" };
        self.control.content.set_visible_child_name(page);

        self.render(&snapshot);
        *self.last_snapshot.borrow_mut() = Some(snapshot);
    }

    /// Draws the selected device on the Control page, and every device in
    /// Preferences.
    fn render(&self, snapshot: &Snapshot) {
        let mac = self.selected.borrow().clone();
        render_device(&self.control, snapshot, mac.as_deref());
        self.devices.update(snapshot);
    }

    /// Draws the last snapshot again.
    fn redraw(&self) {
        if let Some(snapshot) = self.last_snapshot.borrow().as_ref() {
            self.render(snapshot);
        }
    }

    /// Shows the device at `position` in the switcher.
    fn select(&self, position: u32) {
        if self.syncing.get() {
            return;
        }
        // GTK_INVALID_LIST_POSITION on an empty model is out of range for the
        // MAC list, so the lookup simply finds nothing.
        let mac = self
            .control
            .device_macs
            .borrow()
            .get(position as usize)
            .cloned();
        if let Some(mac) = mac {
            *self.selected.borrow_mut() = Some(mac);
            self.redraw();
        }
    }
}

/// The window holds the Control page alone. Preferences and About open from the
/// primary menu, as dialogs, rather than sitting beside it as tabs.
fn setup_ui(win: &adw::ApplicationWindow, control: &ControlView) {
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
