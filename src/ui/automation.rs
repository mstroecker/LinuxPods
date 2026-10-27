//! UI automation over D-Bus, for the test harness (#74). Nothing outside the
//! app can see or press its window: GNOME on Wayland blocks screenshots by
//! other apps, and GTK's accessibility tree is incomplete and gives no
//! coordinates to click at. So the app answers for itself.
//!
//! `io.github.mstroecker.LinuxPods.Automation1` at
//! `/io/github/mstroecker/LinuxPods/Automation`, in harness builds only, and
//! only with `LINUXPODS_HARNESS_AUTOMATION=1`. `scripts/fake-world.sh` sets it
//! for an app it has put on a private session bus. Nothing else does: a
//! harness build is also what `make test` and `make fake-world` leave at
//! `target/debug/linuxpods`, and served on the user's own session, the
//! interface would let any process there press and screenshot the window.
//!
//! It is registered in `startup` on the application's own session bus
//! connection, so only the primary instance has it. GApplication takes the
//! bus name before `startup` runs, and GTK starts up before the interface is
//! registered, so for a moment the name is there and the object is not: a
//! caller retries its first call - `Settle` is the natural one - while it
//! fails with `org.freedesktop.DBus.Error.UnknownMethod`.
//!
//! | Method | Does |
//! |---|---|
//! | `Tree() → s` | Every toplevel window and the widgets in it, as JSON |
//! | `Click(s widget)` | Presses the widget, as a click on it does |
//! | `SetActive(s widget, b active)` | Turns a switch, check or toggle button on or off |
//! | `Select(s widget, u position)` | Selects a dropdown's item through `set_selected`, so `selected-notify` fires as for a user |
//! | `Screenshot(s widget) → s path` | Draws the widget to a PNG in `$XDG_RUNTIME_DIR/linuxpods-harness/` |
//! | `Settle(u timeout_ms)` | Returns once the window has taken in the latest snapshot, and what was opened is on screen |
//!
//! The menu's entries are actions, which GApplication already serves on
//! `org.gtk.Actions`: `app.quit` at the application's path, and on Wayland and
//! X11 `win.preferences`, `win.about` and `win.noise-mode` at the window's,
//! `/io/github/mstroecker/LinuxPods/window/1`. On Broadway GTK exports no
//! window actions; there, open the menu with `Click("main-menu")` and click
//! its item.
//!
//! GDBus calls the handlers on the main context, so they touch widgets
//! directly; nothing here runs on tokio. The tree is read into [`Node`]s, plain
//! data that is turned into JSON and searched without GTK, which is what the
//! tests cover. Only reading a widget and pressing one need a display.
//!
//! ## Widget IDs
//!
//! A method names its widget by ID, or by path when the name starts with `/`.
//! The controls, and the labels a test reads, carry an ID set with
//! `set_widget_name`; anything else goes by its path, which shifts whenever the
//! layout does. IDs are kebab-case and say what a widget shows or does, not
//! what type it is. Parts of one display extend its ID, a noise radio takes its
//! action target, and a widget repeated per device ends in that device's MAC as
//! the UI writes it:
//!
//! | ID | Widget |
//! |---|---|
//! | `main-window` | the window |
//! | `toasts` | the window's toast overlay; a failed command's toast is in it while shown |
//! | `main-menu` | the primary menu's button |
//! | `window-title` | the title, shown while the switcher is not |
//! | `device-dropdown` | the device switcher, shown with more than one device |
//! | `content` | a stack: page `device`, or `empty` while no device is known |
//! | `empty-state` | the empty page |
//! | `battery-left`, `battery-right`, `battery-case` | a column's percentage, `--` when unknown |
//! | `battery-<column>-level` | its level bar |
//! | `battery-<column>-charging`, `battery-<column>-in-ear` | its icons, visible while they apply |
//! | `status` | the status line under the battery display |
//! | `last-seen` | how old a BLE reading is; hidden for AAP |
//! | `noise-control` | the Noise Control group, insensitive unless the device is on AAP |
//! | `noise-off`, `noise-noise-cancelling`, `noise-transparency`, `noise-adaptive` | its radios |
//! | `conversation-awareness` | the Features switch, always insensitive |
//! | `preferences` | the Preferences dialog, while open |
//! | `auto-connect`, `battery-notifications` | its switches, always insensitive |
//! | `device-<MAC>` | a device's row in the Development group |
//! | `device-status-<MAC>` | its status: Connected, Advertising, ... |
//! | `about` | the About dialog, while open |
//!
//! ## The tree
//!
//! `Tree` returns a JSON array of the toplevel windows, oldest first. Dialogs
//! have no window of their own: libadwaita shows them inside the window that
//! opened them, so they are in its tree while they are open. Every widget is
//! listed, a dropdown's button and a row's boxes too, each as an object:
//!
//! - `type`: the GTK type, `GtkLabel`, `AdwActionRow`
//! - `id`: its ID, if it has one
//! - `path`: `/` and an index per level, from the window down: `/0/2/1`
//! - `role`: its accessible role, `label`, `button`, `check-box`
//! - text, where there is some: `label`, `title`, `subtitle`, `description`,
//!   and `icon` for an image's icon name
//! - state, where there is some: `active` for switches, check and toggle
//!   buttons; `selected` and `items` for dropdowns; `value` for level bars,
//!   from 0 to 1; `page` for a stack's visible page
//! - `sensitive`: whether it takes input - false when it or anything holding
//!   it is insensitive, which is also how it is drawn
//! - `visible`: its own `visible` property, the one the UI code sets
//! - `mapped`: whether it is on screen - visible along with everything holding
//!   it, on the stack page shown, in a window that is shown
//! - `children`, if it has any
//!
//! ## Pressing
//!
//! `Click`, `SetActive` and `Select` refuse what a user could not press either:
//! a widget that is not on screen, or one that takes no input. That is what
//! lets a test see Noise Control closed to a device on BLE. A press takes the
//! user's path, and is done when the call returns: `Click` emits a button's
//! `clicked`, pops a menu button's popover up or down, flips a switch, and
//! activates anything else with `gtk_widget_activate`, as Enter on it does.
//! `SetActive` clicks the widget only when its state differs, so a check
//! button bound to an action - a noise radio - activates the action, and the
//! action's state decides. Whatever a press sets off - a command to the
//! AirPods - comes back as a snapshot, when it comes back at all, and as a
//! toast when it fails.
//!
//! ## Settling
//!
//! The window takes in a snapshot at most once per render interval, so a tree
//! read straight after a change can be one snapshot behind. Nor does a popover
//! appear at once: it is on screen only once the display server has placed
//! it, a moment after the click that opened it. And libadwaita puts a dialog's
//! content on screen only as it lays the dialog out, which happens as a frame
//! is drawn - over two frames, when a narrow window turns the dialog into a
//! bottom sheet. `Settle` waits until no snapshot is waiting for the window
//! and every window and popover that has been shown is on screen, then has
//! them draw frames until one changes nothing in the tree. It covers the app's
//! side only: a change still on its way through BlueZ, AAP or the coordinator
//! has not been broadcast, and nothing here can wait for it.
//!
//! Registering the interface turns GTK's animations off. Nothing says when an
//! animation ends, and until a dialog's has begun, its content is not even on
//! screen; without them, every change is complete once it is drawn.
//!
//! `Screenshot` has the window draw a frame first, so it shows the widgets as
//! they are when called. A popover is drawn on a surface of its own, so a
//! screenshot of the window leaves it out; take one of the popover itself.
//!
//! ## Errors
//!
//! Under `io.github.mstroecker.LinuxPods.Automation1.Error.`: `NotFound`;
//! `Ambiguous`, for an ID that more than one widget carries; `Hidden`;
//! `Insensitive`; `Unsupported`, for a press the widget does not take, such as
//! a click on a label; `Timeout`; and `Failed`. A malformed path, or a position
//! past a dropdown's last item, is `org.freedesktop.DBus.Error.InvalidArgs`.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use serde::Serialize;

const INTERFACE_NAME: &str = "io.github.mstroecker.LinuxPods.Automation1";
const OBJECT_PATH: &str = "/io/github/mstroecker/LinuxPods/Automation";

/// Set to `1`, it has [`register`] serve the interface. A variable of its
/// own, rather than the AAP socket directory the fake world also sets: that
/// one says where AAP goes, and neither should switch on the other.
const ENABLE_ENV: &str = "LINUXPODS_HARNESS_AUTOMATION";

const INTERFACE_XML: &str = r#"<node>
  <interface name="io.github.mstroecker.LinuxPods.Automation1">
    <method name="Tree">
      <arg name="json" type="s" direction="out"/>
    </method>
    <method name="Click">
      <arg name="widget" type="s" direction="in"/>
    </method>
    <method name="SetActive">
      <arg name="widget" type="s" direction="in"/>
      <arg name="active" type="b" direction="in"/>
    </method>
    <method name="Select">
      <arg name="widget" type="s" direction="in"/>
      <arg name="position" type="u" direction="in"/>
    </method>
    <method name="Screenshot">
      <arg name="widget" type="s" direction="in"/>
      <arg name="path" type="s" direction="out"/>
    </method>
    <method name="Settle">
      <arg name="timeout_ms" type="u" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Below `$XDG_RUNTIME_DIR`: where screenshots go. The caller never names a
/// path, so a call cannot write anywhere else.
const SCREENSHOT_DIR: &str = "linuxpods-harness";

/// How often `Settle` looks again. Small beside the render interval it waits
/// out, so it adds little to the wait.
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// How long a window gets to draw a frame. A frame takes milliseconds - up to
/// a second on Broadway with no browser attached - and a window that draws
/// none in this time is not drawing at all.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Numbers the screenshots, so that none overwrites another.
static SHOTS: AtomicU32 = AtomicU32::new(1);

thread_local! {
    /// Whether a snapshot is waiting for the window's render loop, as
    /// `window::activate` reports it. Empty until the window is built.
    static SNAPSHOT_WAITING: RefCell<Option<Box<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

/// Registers the interface on the application's own D-Bus connection, when
/// [`ENABLE_ENV`] asks for it. Call it from `startup`: only the primary
/// instance gets there, and by then GApplication has registered on the bus
/// and holds the connection.
pub fn register(app: &impl IsA<gio::Application>) {
    if !enabled(std::env::var_os(ENABLE_ENV).as_deref()) {
        tracing::info!("UI automation off: {ENABLE_ENV} is not 1");
        return;
    }
    let Some(connection) = app.dbus_connection() else {
        tracing::warn!("UI automation unavailable: the application is not on a session bus");
        return;
    };
    let interface = gio::DBusNodeInfo::for_xml(INTERFACE_XML)
        .ok()
        .and_then(|node| node.lookup_interface(INTERFACE_NAME))
        .expect("the interface XML is valid");

    let registered = connection
        .register_object(OBJECT_PATH, &interface)
        .typed_method_call::<Call>()
        .invoke(|_, _, call, invocation| dispatch(call, invocation))
        .build();
    match registered {
        Ok(_) => tracing::info!("UI automation serving {INTERFACE_NAME} at {OBJECT_PATH}"),
        Err(e) => {
            tracing::warn!("failed to register UI automation: {e}");
            return;
        }
    }

    // An animation leaves the UI changing after the call that set it off has
    // returned, and nothing says when it ends. Without them, every change is
    // complete once drawn.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_enable_animations(false);
    }
}

/// Whether the interface is served, given [`ENABLE_ENV`]'s value. Only `1`
/// counts, so that a harness build run any other way serves nothing.
fn enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

/// Lets `Settle` ask the window's render loop whether a snapshot is waiting
/// for it. Called by `window::activate`.
pub(super) fn watch_render_loop(waiting: impl Fn() -> bool + 'static) {
    SNAPSHOT_WAITING.with_borrow_mut(|slot| *slot = Some(Box::new(waiting)));
}

/// A method call, parsed from its arguments.
#[derive(Debug, PartialEq)]
enum Call {
    Tree,
    Click(String),
    SetActive(String, bool),
    Select(String, u32),
    Screenshot(String),
    Settle(u32),
}

impl gio::prelude::DBusMethodCall for Call {
    fn parse_call(
        _path: &str,
        _interface: Option<&str>,
        method: &str,
        params: glib::Variant,
    ) -> Result<Self, glib::Error> {
        // GDBus checks the method and its signature against the interface
        // before it gets here, so neither error is expected.
        let call = match method {
            "Tree" => Some(Call::Tree),
            "Click" => params.get().map(|(widget,)| Call::Click(widget)),
            "SetActive" => params
                .get()
                .map(|(widget, active)| Call::SetActive(widget, active)),
            "Select" => params
                .get()
                .map(|(widget, position)| Call::Select(widget, position)),
            "Screenshot" => params.get().map(|(widget,)| Call::Screenshot(widget)),
            "Settle" => params.get().map(|(timeout_ms,)| Call::Settle(timeout_ms)),
            _ => {
                return Err(glib::Error::new(
                    gio::DBusError::UnknownMethod,
                    &format!("no method {method}"),
                ));
            }
        };
        call.ok_or_else(|| {
            glib::Error::new(
                gio::DBusError::InvalidArgs,
                &format!("{method} does not take {}", params.type_()),
            )
        })
    }
}

/// Runs a call and replies to it.
fn dispatch(call: Call, invocation: gio::DBusMethodInvocation) {
    tracing::debug!("automation: {call:?}");
    match call {
        Call::Tree => reply(invocation, tree().map(|json| Some(json.to_variant()))),
        Call::Click(target) => reply(invocation, click(&target).map(|()| None)),
        Call::SetActive(target, active) => {
            reply(invocation, set_active(&target, active).map(|()| None));
        }
        Call::Select(target, position) => {
            reply(invocation, select(&target, position).map(|()| None));
        }
        // These two wait, and reply once done. Waiting in the handler would
        // stop the main loop, and with it the very window they wait for.
        Call::Screenshot(target) => {
            glib::spawn_future_local(async move {
                let path = screenshot(&target).await;
                reply(invocation, path.map(|path| Some(path.to_variant())));
            });
        }
        Call::Settle(timeout_ms) => {
            glib::spawn_future_local(async move {
                let timeout = Duration::from_millis(timeout_ms.into());
                let look =
                    || (snapshots_taken_in() && !appearing()).then(|| read_tree(&toplevels()));
                let settled = settle(timeout, look, draw_frames).await;
                reply(invocation, settled.map(|()| None));
            });
        }
    }
}

fn reply(invocation: gio::DBusMethodInvocation, result: Result<Option<glib::Variant>, Error>) {
    match result {
        Ok(value) => invocation.return_result(Ok(value)),
        Err(e) => {
            tracing::debug!("automation call refused: {e}");
            invocation.return_dbus_error(e.dbus_name(), &e.to_string());
        }
    }
}

/// Why a call failed, with the D-Bus error it is returned as.
#[derive(Debug, PartialEq, thiserror::Error)]
enum Error {
    #[error("no widget {0}")]
    NotFound(String),
    #[error("{id} names {} widgets: {}", paths.len(), paths.join(", "))]
    Ambiguous { id: String, paths: Vec<String> },
    #[error("{0} is not on screen")]
    Hidden(String),
    #[error("{0} is insensitive")]
    Insensitive(String),
    #[error("{target} ({kind}) cannot {what}")]
    Unsupported {
        target: String,
        kind: String,
        what: &'static str,
    },
    #[error("{0}")]
    InvalidArgs(String),
    #[error("{0}")]
    Timeout(String),
    #[error("{0}")]
    Failed(String),
}

impl Error {
    fn dbus_name(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "io.github.mstroecker.LinuxPods.Automation1.Error.NotFound",
            Error::Ambiguous { .. } => "io.github.mstroecker.LinuxPods.Automation1.Error.Ambiguous",
            Error::Hidden(_) => "io.github.mstroecker.LinuxPods.Automation1.Error.Hidden",
            Error::Insensitive(_) => "io.github.mstroecker.LinuxPods.Automation1.Error.Insensitive",
            Error::Unsupported { .. } => {
                "io.github.mstroecker.LinuxPods.Automation1.Error.Unsupported"
            }
            Error::InvalidArgs(_) => "org.freedesktop.DBus.Error.InvalidArgs",
            Error::Timeout(_) => "io.github.mstroecker.LinuxPods.Automation1.Error.Timeout",
            Error::Failed(_) => "io.github.mstroecker.LinuxPods.Automation1.Error.Failed",
        }
    }
}

/// One widget as the tree records it: what it shows and what state it is in,
/// copied out so that the JSON and every lookup work without GTK. The module
/// docs describe each field.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Node {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Filled in by `number`, once the whole tree is read.
    path: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<String>,
    sensitive: bool,
    visible: bool,
    mapped: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
}

/// Gives every node its path: `parent`, then its index among its siblings.
fn number(nodes: &mut [Node], parent: &str) {
    for (index, node) in nodes.iter_mut().enumerate() {
        node.path = format!("{parent}/{index}");
        number(&mut node.children, &node.path);
    }
}

/// Finds the widget `target` names - by path if it starts with `/`, by ID
/// otherwise - as indices from the windows down, along with its node.
fn locate<'a>(roots: &'a [Node], target: &str) -> Result<(Vec<usize>, &'a Node), Error> {
    let indices = if target.starts_with('/') {
        parse_path(target)?
    } else {
        let mut found = Vec::new();
        find_id(roots, target, &mut Vec::new(), &mut found);
        match found.len() {
            0 => return Err(Error::NotFound(target.to_string())),
            1 => found.remove(0),
            _ => {
                return Err(Error::Ambiguous {
                    id: target.to_string(),
                    paths: found.iter().map(|indices| path_of(indices)).collect(),
                });
            }
        }
    };
    let node = node_at(roots, &indices).ok_or_else(|| Error::NotFound(target.to_string()))?;
    Ok((indices, node))
}

fn parse_path(path: &str) -> Result<Vec<usize>, Error> {
    path.strip_prefix('/')
        .filter(|rest| !rest.is_empty())
        .and_then(|rest| rest.split('/').map(|index| index.parse().ok()).collect())
        .ok_or_else(|| {
            Error::InvalidArgs(format!(
                "{path} is not a path: `/` and an index per level, such as /0/1"
            ))
        })
}

fn path_of(indices: &[usize]) -> String {
    indices.iter().map(|index| format!("/{index}")).collect()
}

/// Collects the indices of every node below `nodes` whose ID is `id`.
fn find_id(nodes: &[Node], id: &str, at: &mut Vec<usize>, found: &mut Vec<Vec<usize>>) {
    for (index, node) in nodes.iter().enumerate() {
        at.push(index);
        if node.id.as_deref() == Some(id) {
            found.push(at.clone());
        }
        find_id(&node.children, id, at, found);
        at.pop();
    }
}

fn node_at<'a>(roots: &'a [Node], indices: &[usize]) -> Option<&'a Node> {
    let (first, rest) = indices.split_first()?;
    rest.iter()
        .try_fold(roots.get(*first)?, |node, &index| node.children.get(index))
}

/// Refuses a press a user could not make either: on a widget that is not on
/// screen, or one that takes no input.
fn pressable(node: &Node, target: &str) -> Result<(), Error> {
    if !node.mapped {
        return Err(Error::Hidden(target.to_string()));
    }
    if !node.sensitive {
        return Err(Error::Insensitive(target.to_string()));
    }
    Ok(())
}

/// The state `SetActive` would change, if the widget has one and a user could
/// press it.
fn toggle_state(node: &Node, target: &str) -> Result<bool, Error> {
    let active = node
        .active
        .ok_or_else(|| unsupported(node, target, "be switched on or off"))?;
    pressable(node, target)?;
    Ok(active)
}

/// Checks a `Select`: a dropdown a user could press, with an item at
/// `position`.
fn selectable(node: &Node, target: &str, position: u32) -> Result<(), Error> {
    let items = node
        .items
        .as_ref()
        .ok_or_else(|| unsupported(node, target, "select an item"))?;
    pressable(node, target)?;
    if usize::try_from(position).is_ok_and(|position| position < items.len()) {
        Ok(())
    } else {
        Err(Error::InvalidArgs(format!(
            "{target} has {} items, none at position {position}",
            items.len()
        )))
    }
}

fn unsupported(node: &Node, target: &str, what: &'static str) -> Error {
    Error::Unsupported {
        target: target.to_string(),
        kind: node.kind.clone(),
        what,
    }
}

/// The screenshot's file name: the target, with anything but ASCII letters,
/// digits and `-` replaced so that neither an ID nor a path can leave the
/// directory, then the process and the shot's number.
fn screenshot_name(target: &str, pid: u32, shot: u32) -> String {
    let stem: String = target
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}-{pid}-{shot}.png")
}

/// Whether the window has taken in every snapshot broadcast so far. Not before
/// it is built: until then, nothing is shown at all.
fn snapshots_taken_in() -> bool {
    SNAPSHOT_WAITING.with_borrow(|waiting| waiting.as_ref().is_some_and(|waiting| !waiting()))
}

/// Waits, without holding up the main loop, for the UI to hold still. `look`
/// is what the UI shows, or None while something is known to be on its way;
/// it holds still once a frame drawn by `draw` changes nothing in it. Layout
/// runs as a frame is drawn, and libadwaita puts some content on screen only
/// as it lays it out: a dialog opens in one frame and shows its content in
/// the next.
async fn settle<S: PartialEq, F: Future<Output = ()>>(
    timeout: Duration,
    look: impl Fn() -> Option<S>,
    draw: impl Fn() -> F,
) -> Result<(), Error> {
    let wait = async {
        loop {
            let before = loop {
                match look() {
                    Some(seen) => break seen,
                    None => glib::timeout_future(SETTLE_POLL).await,
                }
            };
            draw().await;
            if look().is_some_and(|after| after == before) {
                break;
            }
        }
    };
    glib::future_with_timeout(timeout, wait).await.map_err(|_| {
        Error::Timeout(format!(
            "the UI has not settled within {} ms",
            timeout.as_millis()
        ))
    })
}

fn tree() -> Result<String, Error> {
    serde_json::to_string(&read_tree(&toplevels())).map_err(|e| Error::Failed(e.to_string()))
}

fn click(target: &str) -> Result<(), Error> {
    with_target(target, |widget, node| {
        pressable(node, target)?;
        if press(widget) {
            Ok(())
        } else {
            Err(unsupported(node, target, "be clicked"))
        }
    })
}

/// Does what a click on the widget ends in, before returning. Activating a
/// button or a switch - the keyboard's way - would not: a button is held
/// down for a quarter second before it emits `clicked`, and a switch slides
/// over before it flips. False when the widget takes no click.
fn press(widget: &gtk::Widget) -> bool {
    if let Some(button) = widget.downcast_ref::<gtk::Button>() {
        // Toggles a toggle button, and activates an action bound to it.
        button.emit_clicked();
    } else if let Some(menu) = widget.downcast_ref::<gtk::MenuButton>() {
        // Its button is internal, and toggles the popover.
        if menu.popover().is_some_and(|popover| popover.get_visible()) {
            menu.popdown();
        } else {
            menu.popup();
        }
    } else if let Some(switch) = widget.downcast_ref::<gtk::Switch>() {
        switch.set_active(!switch.is_active());
    } else {
        return widget.activate();
    }
    true
}

fn set_active(target: &str, active: bool) -> Result<(), Error> {
    with_target(target, |widget, node| {
        let current = toggle_state(node, target)?;
        if current != active {
            // As for a click: the button toggles, or, bound to an action,
            // activates it, and the action's state then decides.
            press(widget);
        }
        Ok(())
    })
}

fn select(target: &str, position: u32) -> Result<(), Error> {
    with_target(target, |widget, node| {
        selectable(node, target, position)?;
        let dropdown = widget
            .downcast_ref::<gtk::DropDown>()
            .ok_or_else(|| unsupported(node, target, "select an item"))?;
        dropdown.set_selected(position);
        Ok(())
    })
}

async fn screenshot(target: &str) -> Result<String, Error> {
    let widget = with_target(target, |widget, node| {
        if node.mapped {
            Ok(widget.clone())
        } else {
            Err(Error::Hidden(target.to_string()))
        }
    })?;

    // A widget keeps what it drew only until it is next invalidated, so the
    // capture is taken as a fresh frame ends: it shows the widget as it is
    // now, not as it was last drawn.
    let capture = widget.clone();
    let image = after_frame(&widget, move || {
        gtk::WidgetPaintable::new(Some(&capture)).current_image()
    })
    .await
    .ok_or_else(|| {
        Error::Timeout(format!(
            "the window holding {target} drew no frame within {} s",
            FRAME_TIMEOUT.as_secs()
        ))
    })?;
    let texture =
        render(&widget, &image).ok_or_else(|| Error::Failed(format!("{target} drew nothing")))?;

    let dir = glib::user_runtime_dir().join(SCREENSHOT_DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| Error::Failed(format!("cannot create {}: {e}", dir.display())))?;
    let shot = SHOTS.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(screenshot_name(target, std::process::id(), shot));
    texture
        .save_to_png(&path)
        .map_err(|e| Error::Failed(format!("cannot write {}: {e}", path.display())))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The toplevel windows, oldest first, so that a window opened later does not
/// shift the paths of those before it.
fn toplevels() -> Vec<gtk::Widget> {
    gtk::Window::toplevels()
        .iter::<gtk::Widget>()
        .filter_map(Result::ok)
        .collect()
}

/// Every window and popover, shown or not. Each draws on a surface of its own.
fn natives() -> Vec<gtk::Widget> {
    fn collect(widget: &gtk::Widget, found: &mut Vec<gtk::Widget>) {
        if widget.is::<gtk::Native>() {
            found.push(widget.clone());
        }
        for child in children(widget) {
            collect(&child, found);
        }
    }
    let mut found = Vec::new();
    for window in toplevels() {
        collect(&window, &mut found);
    }
    found
}

/// Whether a window or popover has been shown but is not on screen yet. A
/// popover's parent must be on screen for it to be: one inside a hidden
/// widget never arrives.
fn appearing() -> bool {
    natives().iter().any(|native| {
        native.get_visible()
            && !native.is_mapped()
            && native.parent().is_none_or(|parent| parent.is_mapped())
    })
}

/// Has every window and popover on screen draw a frame. One that draws none
/// in time is not waited for any longer: it is not drawing at all.
async fn draw_frames() {
    let shown = natives().into_iter().filter(|native| native.is_mapped());
    futures_util::future::join_all(shown.map(|native| async move {
        after_frame(&native, || ()).await;
    }))
    .await;
}

fn read_tree(windows: &[gtk::Widget]) -> Vec<Node> {
    let mut tree: Vec<Node> = windows.iter().map(read).collect();
    number(&mut tree, "");
    tree
}

/// Reads the tree, finds `target` in it and hands `act` the live widget along
/// with what the tree says of it. Both are read in one go on the main
/// context, so they cannot disagree.
fn with_target<T>(
    target: &str,
    act: impl FnOnce(&gtk::Widget, &Node) -> Result<T, Error>,
) -> Result<T, Error> {
    let windows = toplevels();
    let tree = read_tree(&windows);
    let (indices, node) = locate(&tree, target)?;
    let widget =
        widget_at(&windows, &indices).ok_or_else(|| Error::NotFound(target.to_string()))?;
    act(&widget, node)
}

/// The widget at `indices`, walking the same children in the same order as
/// `read`.
fn widget_at(windows: &[gtk::Widget], indices: &[usize]) -> Option<gtk::Widget> {
    let (first, rest) = indices.split_first()?;
    rest.iter()
        .try_fold(windows.get(*first)?.clone(), |widget, &index| {
            children(&widget).nth(index)
        })
}

fn children(widget: &gtk::Widget) -> impl Iterator<Item = gtk::Widget> + use<> {
    std::iter::successors(widget.first_child(), |child| child.next_sibling())
}

/// Reads a widget, and everything inside it.
fn read(widget: &gtk::Widget) -> Node {
    let kind = widget.type_().name();
    let name = widget.widget_name();
    let mut node = Node {
        kind: kind.to_string(),
        // A widget without a name reports its type's name instead.
        id: (name != kind).then(|| name.to_string()),
        // Read as a property: the bindings' getter wants GTK 4.10.
        role: role_name(widget.property("accessible-role")),
        sensitive: widget.is_sensitive(),
        visible: widget.get_visible(),
        mapped: widget.is_mapped(),
        children: children(widget).map(|child| read(&child)).collect(),
        ..Node::default()
    };
    read_text(widget, &mut node);
    read_state(widget, &mut node);
    node
}

fn read_text(widget: &gtk::Widget, node: &mut Node) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        node.label = text(label.text());
    } else if let Some(button) = widget.downcast_ref::<gtk::Button>() {
        node.label = button.label().and_then(text);
    } else if let Some(check) = widget.downcast_ref::<gtk::CheckButton>() {
        node.label = check.label().and_then(text);
    } else if let Some(image) = widget.downcast_ref::<gtk::Image>() {
        node.icon = image.icon_name().and_then(text);
    }

    if let Some(window) = widget.downcast_ref::<gtk::Window>() {
        node.title = window.title().and_then(text);
    } else if let Some(dialog) = widget.downcast_ref::<adw::Dialog>() {
        node.title = text(dialog.title());
    } else if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
        node.title = text(row.title());
        if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
            node.subtitle = row.subtitle().and_then(text);
        }
    } else if let Some(title) = widget.downcast_ref::<adw::WindowTitle>() {
        node.title = text(title.title());
        node.subtitle = text(title.subtitle());
    } else if let Some(group) = widget.downcast_ref::<adw::PreferencesGroup>() {
        node.title = text(group.title());
        node.description = group.description().and_then(text);
    } else if let Some(page) = widget.downcast_ref::<adw::StatusPage>() {
        node.title = text(page.title());
        node.description = page.description().and_then(text);
    }
}

fn read_state(widget: &gtk::Widget, node: &mut Node) {
    if let Some(switch) = widget.downcast_ref::<gtk::Switch>() {
        node.active = Some(switch.is_active());
    } else if let Some(check) = widget.downcast_ref::<gtk::CheckButton>() {
        node.active = Some(check.is_active());
    } else if let Some(toggle) = widget.downcast_ref::<gtk::ToggleButton>() {
        node.active = Some(toggle.is_active());
    } else if let Some(dropdown) = widget.downcast_ref::<gtk::DropDown>() {
        let selected = dropdown.selected();
        node.selected = (selected != gtk::INVALID_LIST_POSITION).then_some(selected);
        node.items = Some(
            dropdown
                .model()
                .map(|model| items(&model))
                .unwrap_or_default(),
        );
    } else if let Some(level) = widget.downcast_ref::<gtk::LevelBar>() {
        node.value = Some(level.value());
    } else if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
        node.page = stack.visible_child_name().map(Into::into);
    }
}

/// A dropdown's items as text. Every model here is a `StringList`; an item of
/// another kind keeps its place, by its type's name.
fn items(model: &gio::ListModel) -> Vec<String> {
    (0..model.n_items())
        .filter_map(|position| model.item(position))
        .map(|item| match item.downcast_ref::<gtk::StringObject>() {
            Some(string) => string.string().into(),
            None => item.type_().name().to_string(),
        })
        .collect()
}

/// The role's nick, as the accessibility tree names it: `check-box`.
fn role_name(role: gtk::AccessibleRole) -> String {
    glib::EnumValue::from_value(&role.to_value()).map_or_else(
        || format!("{role:?}"),
        |(_, value)| value.nick().to_string(),
    )
}

/// Empty text is no text.
fn text(s: impl AsRef<str>) -> Option<String> {
    let s = s.as_ref();
    (!s.is_empty()).then(|| s.to_string())
}

/// Has the widget's window draw a frame, and runs `then` as the frame ends.
/// None if the window draws none within `FRAME_TIMEOUT`.
async fn after_frame<T: 'static>(
    widget: &gtk::Widget,
    then: impl Fn() -> T + 'static,
) -> Option<T> {
    let clock = widget.frame_clock()?;
    let (tx, rx) = async_channel::bounded(1);
    let handler = clock.connect_after_paint(move |_| {
        let _ = tx.try_send(then());
    });
    // Disconnected however this ends, a timeout included.
    let _connected = AfterPaint {
        clock,
        handler: Some(handler),
    };
    widget.queue_draw();
    glib::future_with_timeout(FRAME_TIMEOUT, rx.recv())
        .await
        .ok()?
        .ok()
}

struct AfterPaint {
    clock: gdk::FrameClock,
    handler: Option<glib::SignalHandlerId>,
}

impl Drop for AfterPaint {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.clock.disconnect(handler);
        }
    }
}

/// Draws a captured frame to a texture at the widget's scale, through the
/// renderer of its window, the one that put it on screen.
fn render(widget: &gtk::Widget, image: &gdk::Paintable) -> Option<gdk::Texture> {
    let (width, height) = (image.intrinsic_width(), image.intrinsic_height());
    if width <= 0 || height <= 0 {
        return None;
    }
    let scale = widget.scale_factor() as f32;
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scale, scale);
    image.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node()?;
    let renderer = widget.native()?.renderer()?;
    let viewport = graphene::Rect::new(0.0, 0.0, width as f32 * scale, height as f32 * scale);
    Some(renderer.render_texture(node, Some(&viewport)))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gio::prelude::DBusMethodCall;
    use serde_json::json;

    use super::*;

    /// A widget of type `kind`, on screen and taking input.
    fn widget(kind: &str, id: Option<&str>, children: Vec<Node>) -> Node {
        Node {
            kind: kind.to_string(),
            id: id.map(str::to_string),
            role: "generic".to_string(),
            sensitive: true,
            visible: true,
            mapped: true,
            children,
            ..Node::default()
        }
    }

    /// A window with a status label, a box holding a switch and an unnamed
    /// label, and a dropdown; and a second window that reuses an ID.
    fn tree() -> Vec<Node> {
        let mut tree = vec![
            widget(
                "AdwApplicationWindow",
                Some("main-window"),
                vec![
                    widget("GtkLabel", Some("status"), vec![]),
                    widget(
                        "GtkBox",
                        None,
                        vec![
                            Node {
                                active: Some(false),
                                ..widget("GtkSwitch", Some("auto-connect"), vec![])
                            },
                            widget("GtkLabel", None, vec![]),
                        ],
                    ),
                    Node {
                        items: Some(vec!["AirPods Pro".into(), "AirPods Max".into()]),
                        selected: Some(0),
                        ..widget("GtkDropDown", Some("device-dropdown"), vec![])
                    },
                ],
            ),
            widget(
                "GtkWindow",
                None,
                vec![widget("GtkLabel", Some("status"), vec![])],
            ),
        ];
        number(&mut tree, "");
        tree
    }

    #[test]
    fn interface_declares_every_method() {
        let node = gio::DBusNodeInfo::for_xml(INTERFACE_XML).expect("valid XML");
        let interface = node
            .lookup_interface(INTERFACE_NAME)
            .expect("the interface");
        for method in [
            "Tree",
            "Click",
            "SetActive",
            "Select",
            "Screenshot",
            "Settle",
        ] {
            assert!(
                interface.lookup_method(method).is_some(),
                "missing {method}"
            );
        }
        assert!(gio::dbus_is_interface_name(INTERFACE_NAME));
        assert!(glib::Variant::is_object_path(OBJECT_PATH));
    }

    /// A harness build on the user's own session - the one `make test` leaves
    /// behind, run against real AirPods - must not open its window to every
    /// process there. Only the fake world asks.
    #[test]
    fn serves_only_when_asked_to() {
        assert!(enabled(Some(OsStr::new("1"))));
        for value in [None, Some(""), Some("0"), Some("true"), Some("1 ")] {
            assert!(!enabled(value.map(OsStr::new)), "{value:?}");
        }
    }

    #[test]
    fn calls_parse_from_their_arguments() {
        let parse = |method, params: glib::Variant| {
            Call::parse_call(OBJECT_PATH, Some(INTERFACE_NAME), method, params)
        };
        assert_eq!(parse("Tree", ().to_variant()).unwrap(), Call::Tree);
        assert_eq!(
            parse("SetActive", ("noise-off", true).to_variant()).unwrap(),
            Call::SetActive("noise-off".into(), true)
        );
        assert_eq!(
            parse("Select", ("device-dropdown", 1u32).to_variant()).unwrap(),
            Call::Select("device-dropdown".into(), 1)
        );
        assert_eq!(
            parse("Settle", (500u32,).to_variant()).unwrap(),
            Call::Settle(500)
        );
        assert!(parse("Select", ("device-dropdown",).to_variant()).is_err());
        assert!(parse("Explode", ().to_variant()).is_err());
    }

    /// Paths count from the windows down, and every one of them leads back to
    /// its node.
    #[test]
    fn paths_number_every_widget() {
        let tree = tree();
        assert_eq!(tree[0].path, "/0");
        assert_eq!(tree[0].children[1].children[1].path, "/0/1/1");
        assert_eq!(tree[1].children[0].path, "/1/0");

        fn check(roots: &[Node], nodes: &[Node]) {
            for node in nodes {
                let (indices, found) = locate(roots, &node.path).unwrap();
                assert_eq!(path_of(&indices), node.path);
                assert_eq!(found, node);
                check(roots, &node.children);
            }
        }
        check(&tree, &tree);
    }

    #[test]
    fn ids_name_one_widget() {
        let tree = tree();
        let (indices, node) = locate(&tree, "auto-connect").unwrap();
        assert_eq!(indices, [0, 1, 0]);
        assert_eq!(node.kind, "GtkSwitch");

        assert_eq!(
            locate(&tree, "battery-left").unwrap_err(),
            Error::NotFound("battery-left".into())
        );
        assert_eq!(
            locate(&tree, "status").unwrap_err(),
            Error::Ambiguous {
                id: "status".into(),
                paths: vec!["/0/0".into(), "/1/0".into()],
            }
        );
    }

    #[test]
    fn paths_must_lead_somewhere() {
        let tree = tree();
        assert_eq!(locate(&tree, "/0/1/1").unwrap().0, [0, 1, 1]);
        assert_eq!(
            locate(&tree, "/0/9").unwrap_err(),
            Error::NotFound("/0/9".into())
        );
        for malformed in ["/", "/a", "/0//1", "/0/", "/-1"] {
            assert!(
                matches!(locate(&tree, malformed), Err(Error::InvalidArgs(_))),
                "{malformed} parsed"
            );
        }
    }

    /// Absent text and state are left out rather than written as null, and so
    /// is an empty list of children.
    #[test]
    fn json_leaves_out_what_a_widget_lacks() {
        let tree = tree();
        let window = serde_json::to_value(&tree[0]).unwrap();
        assert_eq!(
            window["children"][2],
            json!({
                "type": "GtkDropDown",
                "id": "device-dropdown",
                "path": "/0/2",
                "role": "generic",
                "selected": 0,
                "items": ["AirPods Pro", "AirPods Max"],
                "sensitive": true,
                "visible": true,
                "mapped": true,
            })
        );
        assert_eq!(
            window["children"][1]["children"][1],
            json!({
                "type": "GtkLabel",
                "path": "/0/1/1",
                "role": "generic",
                "sensitive": true,
                "visible": true,
                "mapped": true,
            })
        );
        let level = Node {
            value: Some(0.5),
            label: Some("50%".into()),
            ..Node::default()
        };
        let level = serde_json::to_value(&level).unwrap();
        assert_eq!(level["value"], json!(0.5));
        assert_eq!(level["label"], json!("50%"));
    }

    /// Off screen goes first: a user could not even see whether the widget is
    /// greyed out.
    #[test]
    fn presses_refuse_what_a_user_could_not_press() {
        let shown = widget("GtkCheckButton", Some("noise-off"), vec![]);
        assert_eq!(pressable(&shown, "noise-off"), Ok(()));

        let insensitive = Node {
            sensitive: false,
            ..shown.clone()
        };
        assert_eq!(
            pressable(&insensitive, "noise-off"),
            Err(Error::Insensitive("noise-off".into()))
        );

        let hidden = Node {
            mapped: false,
            ..insensitive
        };
        assert_eq!(
            pressable(&hidden, "noise-off"),
            Err(Error::Hidden("noise-off".into()))
        );
    }

    #[test]
    fn only_widgets_with_a_state_switch() {
        let tree = tree();
        let switch = locate(&tree, "auto-connect").unwrap().1;
        assert_eq!(toggle_state(switch, "auto-connect"), Ok(false));

        let greyed = Node {
            sensitive: false,
            ..switch.clone()
        };
        assert_eq!(
            toggle_state(&greyed, "auto-connect"),
            Err(Error::Insensitive("auto-connect".into()))
        );

        let label = locate(&tree, "/0/0").unwrap().1;
        assert_eq!(
            toggle_state(label, "/0/0").unwrap_err().to_string(),
            "/0/0 (GtkLabel) cannot be switched on or off"
        );
    }

    #[test]
    fn selection_stays_within_the_items() {
        let tree = tree();
        let dropdown = locate(&tree, "device-dropdown").unwrap().1;
        assert_eq!(selectable(dropdown, "device-dropdown", 1), Ok(()));
        assert!(matches!(
            selectable(dropdown, "device-dropdown", 2),
            Err(Error::InvalidArgs(_))
        ));

        // With a single device the switcher is hidden, and cannot be used.
        let hidden = Node {
            mapped: false,
            ..dropdown.clone()
        };
        assert_eq!(
            selectable(&hidden, "device-dropdown", 0),
            Err(Error::Hidden("device-dropdown".into()))
        );

        let switch = locate(&tree, "auto-connect").unwrap().1;
        assert!(matches!(
            selectable(switch, "auto-connect", 0),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn screenshots_stay_in_their_directory() {
        assert_eq!(
            screenshot_name("battery-left", 42, 1),
            "battery-left-42-1.png"
        );
        assert_eq!(screenshot_name("/0/2/1", 42, 2), "_0_2_1-42-2.png");
        assert_eq!(screenshot_name("../../etc", 42, 3), "______etc-42-3.png");
        assert_eq!(
            screenshot_name("device-status-AA:BB:CC:DD:EE:FF", 42, 4),
            "device-status-AA_BB_CC_DD_EE_FF-42-4.png"
        );
        assert!(screenshot_name(&"x".repeat(1000), 42, 5).len() < 100);
    }

    #[test]
    fn every_error_has_a_valid_dbus_name() {
        let errors = [
            Error::NotFound(String::new()),
            Error::Ambiguous {
                id: String::new(),
                paths: Vec::new(),
            },
            Error::Hidden(String::new()),
            Error::Insensitive(String::new()),
            Error::Unsupported {
                target: String::new(),
                kind: String::new(),
                what: "",
            },
            Error::InvalidArgs(String::new()),
            Error::Timeout(String::new()),
            Error::Failed(String::new()),
        ];
        // Error names follow the rules for interface names; GLib's own check
        // for them needs a newer GLib than the bindings are built against.
        for error in errors {
            assert!(gio::dbus_is_interface_name(error.dbus_name()), "{error:?}");
        }
    }

    /// Settle returns once the render loop has caught up, and not before; with
    /// no window yet, there is nothing to catch up with. The thread-local is
    /// per test thread, so this test has it to itself.
    #[test]
    fn settle_waits_for_the_render_loop() {
        let context = glib::MainContext::new();
        let settle = |timeout| {
            let look = || snapshots_taken_in().then_some(());
            context.block_on(settle(timeout, look, || std::future::ready(())))
        };

        assert!(matches!(
            settle(Duration::from_millis(30)),
            Err(Error::Timeout(_))
        ));

        let waiting_for = Rc::new(Cell::new(3_u32));
        let polls = waiting_for.clone();
        watch_render_loop(move || {
            let left = polls.get();
            polls.set(left.saturating_sub(1));
            left > 0
        });
        assert_eq!(settle(Duration::from_secs(5)), Ok(()));
        assert_eq!(waiting_for.get(), 0);

        watch_render_loop(|| true);
        assert!(matches!(
            settle(Duration::from_millis(30)),
            Err(Error::Timeout(_))
        ));
    }

    /// A UI that changes as frames are drawn - a dialog laying itself out - is
    /// waited out until a frame leaves it as it was.
    #[test]
    fn settle_waits_for_a_frame_that_changes_nothing() {
        let context = glib::MainContext::new();
        let frames = Rc::new(Cell::new(0_u32));
        let drawn = frames.clone();
        let look = || Some(frames.get().min(3));
        let draw = || {
            drawn.set(drawn.get() + 1);
            std::future::ready(())
        };
        assert_eq!(
            context.block_on(settle(Duration::from_secs(5), look, draw)),
            Ok(())
        );
        assert_eq!(frames.get(), 4);
    }
}
