# Architecture

How Cymba is put together: its two threads, the channels between them, the tasks
`main.rs` starts, and the coordinator they all meet at. A map: each part has a document
of its own, linked below. The source tree, file by file, is in
[AGENTS.md](../../AGENTS.md#architecture).

## Two threads

GTK owns the main thread. A tokio runtime beside it carries BLE, AAP and every D-Bus
call. GTK types are `!Send`, so no widget can travel to a tokio task, and none may be
touched from one: the two sides meet over channels only.

What comes to the main thread is consumed there with `glib::spawn_future_local`. What
goes the other way is spawned on the runtime.

## The channels

| What | From | To | Channel |
|---|---|---|---|
| Snapshots of every pair's state | the coordinator | the window, the tray, attachment, the notifier | `tokio::sync::watch` (`Coordinator::subscribe`): [state.md](state.md#snapshots) |
| The tray's window commands (show, quit) | the tray task | the main context | `async-channel` |
| Low battery notices | `notify::run` | the main context, which sends them: [notifications.md](notifications.md) | `async-channel` |
| A quit asked for by SIGTERM or SIGINT | `quit_on_signals` | the main context, which quits as the menu does | `async-channel` |
| Commands to the AirPods | the window, the tray | the pair's link | its queue (`Coordinator::command`), the result back on a `oneshot`: [commands.md](commands.md) |
| The Preferences switch for pausing on the ears | the main context | `media::Media` | `watch`: [media-pause.md](media-pause.md) |
| Speech levels | the coordinator | `volume::Volume` | events (`Coordinator::speech_events`): [state.md](state.md#events) |
| Each pod's place | the coordinator | `media::Media` | events (`Coordinator::ear_events`): [state.md](state.md#events) |
| How the adapters listen | the scanner | the Preferences scanning hint, on the main context | `watch` (`ble::scanner::ScanControl::state`, the `ScanControl` made in `Primary::start`): [ui.md](ui.md#the-scanning-hint) |
| A discovery pause asked for | the hint's button, on the main context | the scanner | `watch` in `ScanControl`: [bluez.md](bluez.md#listening-for-the-pods) |

## The tasks

`main.rs` starts these on the runtime, once, in the primary instance:

| Task | What it does | Document |
|---|---|---|
| `ble_task` | An advertisement monitor on every powered adapter, discovery where there is none; each advertisement to the coordinator | [identification.md](identification.md), [bluez.md](bluez.md) |
| `attachment::run` | Which pairs are attached; a supervisor per attached pair, which keeps its AAP link up; the GNOME Settings batteries | [aap-links.md](aap-links.md), [battery.md](battery.md), [bluez.md](bluez.md) |
| `expiry_task` | Each minute, prunes the BLE cache and saves the readings when due | [state.md](state.md#the-ble-cache), [readings.md](readings.md) |
| `volume::Volume::run` | Lowers the pair's volume while the user speaks | [volume.md](volume.md) |
| `media::Media::run` | Pauses the media when a pod leaves an ear | [media-pause.md](media-pause.md) |
| `notify::run` | Works out the low battery notices | [notifications.md](notifications.md) |
| `tray_task` | The system tray | [ui.md](ui.md) |

`main()` spawns `quit_on_signals` on every launch, outside that set: it turns SIGTERM and
SIGINT into a quit, as the menu's.

The window's render loops run on the main context: [ui.md](ui.md#updates-come-from-snapshots).

At the quit, `main.rs` puts back a lowered volume, for `volume::RESTORE_LIMIT` at most
([volume.md](volume.md)), and saves the readings once more, without a bound
([readings.md](readings.md)). Both run before the runtime is dropped. A second SIGTERM or
SIGINT exits at once.

## The coordinator

`podstate::Coordinator` holds every pair's state and is the one place it changes. It
merges two sources per pair:

| Source | When | How exact |
|---|---|---|
| AAP, over L2CAP | While a link to the pair is up: [aap-links.md](aap-links.md) | Exact; carries in-ear, the noise mode, the settings and the pods' name too |
| BLE advertisements | Whenever the pair is in range, connected or not | Exact once identified and decrypted: [identification.md](identification.md) |

```
Coordinator (state per pair, keyed by real MAC)
    ├─ AAP links ──────────> exact readings for every connected pair, one link each
    ├─ BLE scanner ────────> identified advertisements from every pair in range
    ├─ Per-pair choice ────> a link's reading supersedes BLE for its own pair only
    ├─ Snapshots (watch) ──> window, tray, attachment (GNOME Settings), notifier
    └─ Events ─────────────> speech levels to the volume, places to the media pause
```

State is keyed by the pair's real MAC, which is what lets a pod's rotating address
collapse onto one pair: [identification.md](identification.md). How the merge works:
[state.md](state.md).

## Where to read on

| Area | Document |
|---|---|
| State, snapshots, events, the BLE cache | [state.md](state.md) |
| Which advertisements count | [identification.md](identification.md) |
| The keys | [keystore.md](keystore.md) |
| Readings across a relaunch | [readings.md](readings.md) |
| AAP links | [aap-links.md](aap-links.md) |
| Commands to the AirPods | [commands.md](commands.md) |
| BlueZ, zbus, adapters, battery providers | [bluez.md](bluez.md) |
| Battery levels and GNOME Settings | [battery.md](battery.md) |
| In-ear | [in-ear.md](in-ear.md) |
| Noise control | [noise-control.md](noise-control.md) |
| Conversation Awareness | [conversation-awareness.md](conversation-awareness.md) |
| The volume lowered while the user speaks | [volume.md](volume.md) |
| The media paused when a pod leaves an ear | [media-pause.md](media-pause.md) |
| Names and rename | [names.md](names.md) |
| Firmware | [firmware.md](firmware.md) |
| Low battery notifications | [notifications.md](notifications.md) |
| The window, Preferences, the tray | [ui.md](ui.md) |
| Logs | [logging.md](logging.md) |
| What LinuxPods left behind | [from-linuxpods.md](from-linuxpods.md) |

## Hardware checks

None open. The parts above have their own checks.
