# UI

How the window, Preferences and the tray get what they show, and the GTK traps they
avoid. The window is `ui`, the tray `indicator`. What each feature shows is in its own
document; this one covers what holds for all of them.

## Updates come from snapshots

**Widgets change only from the snapshot stream** ([state.md](state.md#snapshots)), and
the scanning hint from the scanner's ([The scanning hint](#the-scanning-hint)). The
window's render loops, one per stream, run on the main context, the only place a widget
may be touched ([architecture.md](architecture.md#two-threads)). Each:

- **One pass per `RENDER_INTERVAL` at most.** tokio's cooperative budget does not reach
  glib's executor: under a stream of updates `changed()` would stay ready, and the loop
  would never yield to GTK. The pause caps rendering, not delivery: the next pass picks up
  whatever is newest.
- **The borrow is cloned out** before anything is awaited: it is a read lock every
  broadcast waits for.
- **Draws the current value at once:** `Coordinator::subscribe` and `ScanControl::state`
  hand out receivers marked changed, so a window built after the value was set still
  draws it.

A clock redraws the window every 30 seconds, so a BLE reading's age moves on with no
snapshot. Under `RECENT` it reads "Last seen just now".

**The one exception is a command that fails** ([commands.md](commands.md#what-the-caller-hears)).
Its result comes back to the main context and goes up as a toast: an event, never a
control's state. So the noise control radios keep showing the recorded mode.

## Controls that send commands

Noise Control, Conversation Awareness and the Info group's Name row send commands, so
they are insensitive unless the pair shown is on AAP. BLE is receive-only.

| Control | Sensitive while the pair is on AAP and |
|---|---|
| Noise Control | always |
| The Conversation Awareness switch | the setting has been reported: a switch cannot show unknown |
| The Name row | the pods' name is known: [names.md](names.md#two-names) |

**A snapshot turns the switch under `ControlView::syncing_features`,** so its
`notify::active` does not send the setting back out. A failed command draws the last
snapshot again, which puts the switch back.

The Firmware row below Name is no command: [firmware.md](firmware.md#what-the-app-does-with-it).

## The scanning hint

**Preferences says while Cymba listens by discovery** (#189). While a powered adapter
has no advertisement monitor (`ScanState::fallback`), a group at the top of Preferences
says that mice and similar devices may not reconnect while the app scans. Why they do
not: [bluez.md](bluez.md#listening-for-the-pods). Hidden, it takes no space.

| Row | Does |
|---|---|
| The guide row | Names the fix, BlueZ's experimental features, and opens the user document [other-devices.md](../user/other-devices.md) on GitHub's `main`, in the browser |
| The pause row | Its button calls `ScanControl::pause_discovery`: the app's discovery stops for `DISCOVERY_PAUSE`. Its title reads "Scanning", or "Paused until" and the local time the pause ends, from `ScanState::paused_until`. Pressed again while paused, the pause runs that long from then. |

The pause row's subtitle says that the pause lets other devices connect. That AirPods
nearby go unheard meanwhile, and that another client's discovery, such as GNOME Settings'
Bluetooth panel, still blocks, are left to the user document
([bluez.md](bluez.md#listening-for-the-pods)).

**A second stream, not part of the snapshot.** The scanner's state is no pair's state,
and the window alone reads it. Carried in the snapshot, every change would wake the tray,
attachment and the notifier for nothing, and the coordinator would hold state it does
not merge. So the window follows `ScanControl::state` as it follows the snapshots, with
the same loop, and the harness's `Settle` waits for both.

**The pause shows only once the scanner reports it.** The button changes nothing on
screen: the scanner stops its discovery and sends the new state, which draws the pause,
as a command's result comes back with the snapshot.

**The time the pause ends, not a countdown.** A countdown would redraw every second from a
clock, outside either stream. The end is drawn once; when it comes, the scanner sends a
new state, and the row reads "Scanning" again. A pause whose end has passed before that
state arrives already reads "Scanning".

**Not damped.** During a graceful bluetoothd restart, `fallback` can be true for a moment:
a `Release` can come before the adapter watch hears the adapters power off
([bluez.md](bluez.md#hardware-checks)). The hint would then show for that moment. It is
not held back, as:

- the scanner does fall back to discovery then, though its starts fail, so the hint says
  what is so;
- a delay before showing would hold the hint back wherever it is true too, by a length
  no run has measured;
- Preferences is seldom open while bluetoothd restarts: a guess.

## Drop-downs

**Read a selection before mutating the widget.** Splicing a `StringList` makes
`GtkDropDown` emit `selected-notify`, which clobbers the user's choice.

## Settings

**A setting is not device state.** Its switch is bound to an app action and shows that
action's state:

| Preferences switch | Action | Setting | Document |
|---|---|---|---|
| Battery notifications | `app.battery-notifications` | `Settings::battery_notifications` | [notifications.md](notifications.md#the-switch) |
| Pause when a pod is taken out | `app.pause-on-ear` | `Settings::pause_on_ear` | [media-pause.md](media-pause.md) |

Both are on by default, also for a file from before the setting.

**Kept in `~/.config/cymba/settings.json`,** a file rather than GSettings: a GSettings
schema has to be compiled and installed wherever the app runs, and GIO aborts the process
when it is missing, as it is for a plain `cargo run`.

- **Loaded at startup** by `settings::open`, before any action is created, so a switch
  saved off starts off and sends nothing.
- **Saved by `settings::Saver`** on tokio's blocking pool, never the main loop: the latest
  settings, whatever order the saves run in.
- **A file that does not parse is set aside,** and the defaults used. A field this build
  does not know is kept and written back.
- **A save that fails** keeps the change in memory.

A planned setting not built yet is shown insensitive, with the subtitle "Not available
yet": it neither claims a state nor takes a click.

## Device color

The color byte and its names:
[protocol/ble/proximity-pairing.md](../protocol/ble/proximity-pairing.md#byte-7-device-color).

The Control page shows the color after the model name, as `AirPods Pro 3 • White`. A code
missing from the table shows as `Unknown color (0x..)`.

## The tray

A StatusNotifierItem (`indicator`, through ksni), run by `tray_task` from the snapshots.
It shows the lower of the two pods ([battery.md](battery.md#where-it-goes)), and its menu
sets the noise control mode, logging a command that fails.

## Resources

**Artwork and the app icon are compiled into a GResource** by `build.rs`, from
`assets/resources.gresource.xml`. A missing resource fails silently: `gtk::Image` renders
nothing, and the About dialog shows a placeholder. So the `bundled_resources_resolve`
test asserts that every name the UI loads is in the bundle.

**The tray points the shell at `assets/icons`** in the checkout: the shell draws the tray
icon, and cannot read resources inside the binary.

## Hardware checks

None open: the UI has no hardware of its own. The features' checks are in their
documents, the scanning hint's in [bluez.md](bluez.md#hardware-checks).
