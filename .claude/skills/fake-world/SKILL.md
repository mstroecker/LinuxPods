---
name: fake-world
description: Run LinuxPods against fake AirPods and a fake BlueZ on private D-Bus buses, change the fake devices over D-Bus, and read, press and screenshot the app's UI through its Automation1 interface. Use when asked to run, try, test or screenshot the app without real AirPods; to reproduce a device situation such as a pair connected or in range, two pairs, a BLE flood, a re-pairing or a dropped AAP connection; or to check a UI or protocol change end to end in the real app.
---

# Fake world

`make fake-world` runs the real app - scanner, coordinator, keystore, UI - against
`fakepods`: a fake BlueZ and fake AirPods on a private bus standing in for the system
bus, with each pair's AAP on a Unix socket in place of L2CAP. The app runs on a second
private bus as its session bus. Change the devices through `FakeWorld1`; read and press
the UI through `Automation1`.

Reference: `src/harness/world.rs` (FakeWorld1), `src/harness/bluez.rs` (what the fake
BlueZ signals), `src/ui/automation.rs` (Automation1, the tree's JSON and every widget
ID).

## Safety

The user may be logged in with real AirPods and their own LinuxPods running.

- Talk to the fake world only through `scripts/fake-world.sh world|app|tree`, or pass
  `--address` from its `env` file. Bare `gdbus` and `busctl` go to the real buses.
- Run with `HEADLESS=1` unless the user asks to see the window on their desktop.
- Stop the world when done. `stop` kills only the PIDs that `start` recorded; never
  kill a `linuxpods` process by name, since it may be the user's.

## Start

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only make fake-world SCENARIO=connected HEADLESS=1
```

This builds with `--features harness`, so the binaries are fresh, then returns once the
app answers `Automation1`. It prints the Broadway URL (`http://127.0.0.1:<port>`) and
where the logs are. Only one world runs at a time.

| `SCENARIO` | World |
|---|---|
| `in-range` (default) | `00:00:5E:00:53:01`, AirPods Pro 3, in range and disconnected: the app reads it over BLE |
| `connected` | the same pair connected: the app opens AAP to it at startup |
| `two-pairs` | that one connected, and `00:00:5E:00:53:02`, AirPods Pro 2 (USB-C), in range |
| `flood` | the in-range pair among 20 new undecryptable advertisers a second |
| `empty` | nothing; add pairs with `AddPods` |

Each new pair starts with both pods in the open case: left 80%, right 70%, case 50%,
Noise Cancelling. A scenario's pairs have their keys saved in the app's keystore before
it starts, as though each had connected once before. A pair added later with `AddPods`
has no key until it connects, so until then the app shows nothing of it.

## Change the world

```sh
scripts/fake-world.sh world SetBattery 00:00:5E:00:53:01 left 42
scripts/fake-world.sh world SetBattery 00:00:5E:00:53:01 case 255    # above 100: unknown
scripts/fake-world.sh world Disconnect 00:00:5E:00:53:01
scripts/fake-world.sh world SetQuirk 00:00:5E:00:53:01 silent_handshake '<true>'
scripts/fake-world.sh world Received 00:00:5E:00:53:01
```

Arguments are `gdbus call` text: strings bare, variants as `'<true>'` or `'<4>'`.

| Method | Does |
|---|---|
| `AddPods(s mac, q model, s alias)` | pairs new AirPods, model by product ID: `0x2027` Pro 3, `0x2024` Pro 2 (USB-C) |
| `Connect(s mac)`, `Disconnect(s mac)` | the Bluetooth link; the app attaches or detaches, and disconnecting closes AAP |
| `SetBattery(s mac, s part, y level)` | `left`, `right` or `case` |
| `SetLid(s mac, b open)` | the case lid |
| `Repair(s mac)` | new keys, sent at the next key request; the old key stops decrypting BLE |
| `HangUp(s mac)` | closes AAP from the pods' end; the link stays, and the app does not reconnect (#49) |
| `SetQuirk(s mac, s quirk, v value)` | `off_needs_allow`, `settings_changed`, `echo_every`, `echo_delay_ms`, `zero_suffix_when_connected`, `silent_handshake` |
| `Flood(u count)` | that many advertisers no key decrypts |
| `Received(s mac) → aay` | every AAP packet the app sent the pair: check `0x34 01` precedes Off |
| `Batteries() → a{sy}` | what GNOME Settings shows, by MAC |

## Drive the UI

```sh
scripts/fake-world.sh app Settle 10000
scripts/fake-world.sh tree | jq '.. | objects | select(.id? == "battery-left") | .label'
scripts/fake-world.sh app SetActive noise-off true
scripts/fake-world.sh app Select device-dropdown 1
scripts/fake-world.sh app Screenshot main-window    # prints the PNG's path: Read it
```

`Tree` lists every widget with its `id` (if any), `path`, text and state;
`src/ui/automation.rs` lists the IDs. A press is refused with `Hidden` or `Insensitive`
where a user's click would fail, so Noise Control refuses presses while the device is
on BLE.

Change, settle, then read: `SetBattery`, `Settle`, `tree`.

## Practical notes

- **Settle covers the app's side only.** A change still on its way through the fake
  BlueZ or AAP is not waited for. An AAP change arrives within milliseconds. A BLE
  change waits for the next advertisement, one a second.
- **Settle's timing.** Headless Broadway draws about one frame a second, and Settle
  waits for a frame that changes nothing, so allow several seconds (`Settle 10000`).
  Opening the Preferences dialog took 2 s.
- **Automation1 is served only when asked for.** `start` runs the app with
  `LINUXPODS_HARNESS_AUTOMATION=1`; without it a harness build serves none, and logs
  `UI automation off`. Set it only for an app on a private session bus, never the
  user's.
- **The first call races the app's start.** GApplication takes its name before
  `Automation1` exists, so an early call fails with `UnknownMethod`. `start` already
  retries `Settle` until it answers. An app started another way needs
  `LINUXPODS_HARNESS_AUTOMATION=1`, or it never answers; with it, retry likewise.
- **Window actions are not on D-Bus under Broadway.** GTK exports them only on Wayland
  and X11, so `win.preferences` cannot be activated. Open the menu and click the item
  by its tree path instead - the path of the button holding its label:

  ```sh
  scripts/fake-world.sh app Click main-menu
  scripts/fake-world.sh app Settle 10000
  label=$(scripts/fake-world.sh tree | jq -r '.. | objects | select(.label? == "Preferences") | .path')
  scripts/fake-world.sh app Click "${label%/*}"
  scripts/fake-world.sh app Settle 10000
  ```

  `app.quit` does work: `gdbus call --address "$SESSION_BUS" --dest
  io.github.mstroecker.LinuxPods --object-path /io/github/mstroecker/LinuxPods
  --method org.gtk.Actions.Activate quit '[]' '{}'`.
- **Popovers are separate surfaces.** A screenshot of `main-window` leaves an open menu
  out; take one of the popover by its path. Screenshots go to
  `$XDG_RUNTIME_DIR/linuxpods-harness/`.
- **BLE timing.** Each pair advertises once a second from a random address that
  rotates every 10 s, and the fake BlueZ drops an address 30 s after it goes quiet. An
  address's first advertisement comes in `InterfacesAdded`, the rest in
  `PropertiesChanged`, and the app reads both: 10 readings out of 10 per address, and a
  first reading within about a second of start.
- **Logs** are in `$XDG_RUNTIME_DIR/linuxpods-harness/world/log/`: `linuxpods.log` at
  `RUST_LOG=linuxpods=debug`, `fakepods.log` and `broadwayd.log`. Set `RUST_LOG` before
  `make` to change the level. They stay until the next start.
- **Watching the buses:** `. "$XDG_RUNTIME_DIR/linuxpods-harness/world/env"`, then
  `dbus-monitor --address "$SYSTEM_BUS"` or `busctl --address="$SYSTEM_BUS" tree org.bluez`.

## Stop

```sh
make fake-world-stop
ps -eo pid,args | awk '/target\/debug\/(linuxpods|fakepods)|gtk4-broadwayd|linuxpods-harness\/world/ && !/awk/'
```

The `ps` lists nothing once the world has stopped. (`pgrep -f` with the same pattern
also matches the shell that runs it.)
