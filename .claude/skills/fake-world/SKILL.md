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
Noise Cancellation, Conversation Awareness off. A scenario's pairs have their keys - IRK and ENC_KEY - saved in the
app's keystore before it starts, as though each had connected once before. A pair added
later with `AddPods` has no keys until it connects, so until then the app shows nothing of
it.

The fake BlueZ has one adapter, powered, at `/org/bluez/hci0`. `ADAPTER=hci1` puts it
at `/org/bluez/hci1` instead, as on a machine whose only adapter is not the first
(#51); any scenario runs on it, and `AddPods` pairs with it:

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only make fake-world SCENARIO=connected ADAPTER=hci1 HEADLESS=1
```

`fakepods` starts with one adapter; plug in more, power them off and on, unplug them and
restart the fake BlueZ at runtime with the adapter methods below (#50). The pipeline
tests can also start with several, or one that is off (`World::with_adapters`).

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
| `AddPods(s mac, q model, s alias)` | pairs new AirPods with the first adapter there is now, model by product ID: `0x2027` Pro 3, `0x2024` Pro 2 (USB-C) |
| `Connect(s mac)`, `Disconnect(s mac)` | the Bluetooth link; the app attaches or detaches, and disconnecting closes AAP right after BlueZ's `Connected=false`, as the hardware's AAP dies with the link. BlueZ signals `ServicesResolved=true` 4 s after `Connected=true`, and `false` just before `Connected=false`, as bluetoothd does; nothing in the app waits on it |
| `SetBattery(s mac, s part, y level)` | `left`, `right` or `case` |
| `SetLid(s mac, b open)` | the case lid; closed on both pods in the case, the pods drop their link, as real ones do. Over BLE, a pod in the closed case stops advertising 4 s after it closes; opened, the pods come back under new addresses, the right one 1 s later and the left 5.5 s |
| `Repair(s mac)` | new keys, sent at the next key request; the old IRK stops resolving the pair's addresses, and the old key stops decrypting BLE |
| `HangUp(s mac)` | closes AAP from the pods' end; the link stays, and the app links the pair again after a second, longer after repeated hang-ups (#49) |
| `SetConversationAwareness(s mac, b on)` | the setting, as from a phone: nothing reports it over AAP; the next connection's startup dump carries it |
| `Speak(s mac, u length_ms)` | the user speaks that long. With Conversation Awareness on, the pods send speech levels and switch to Transparency and back at the #150 run's times: level `01`, the mode in use reported 20 ms later, Transparency at 0.31 s, `02` at 0.82 s; at the end `03`, `07`, `08` within 0.1 s, the mode back at 0.46 s and `09` at 0.48 s. With it off, nothing. Fails without AAP, or while a stretch is under way |
| `SetPlaying(s mac, b playing)` | audio to a connected pair starts or stops, as PipeWire's: its A2DP transport (`org.bluez.MediaTransport1` at `<device>/sep1/fd<n>`, a new `n` to the pair with each setup - bluetoothd numbers them from the count of all its transports instead) `active` at the pair's own volume, 64 at first; or `idle`. Fails unless connected |
| `SetCall(s mac, b on)` | a call begins or ends: A2DP `idle`, as a call is assumed to leave it, and playing again after if it did |
| `SetVolume(s mac, q volume)` | the user sets the pair's A2DP volume, 0 to 127 - GNOME's slider, say; not among `VolumeSets`. With no transport, the volume the next one starts at, as a pair back at another volume |
| `SetVolumeAnswer(s mac, u delay_ms, b to_even)` | the pair answers a volume the app sets that long later, rounded down to even if asked, as bluetoothd has real pods answer: the transport reads the old volume until then. A set of the volume held does nothing, as in bluetoothd |
| `Volume(s mac) → q` | the volume of the pair's A2DP transport; fails without one |
| `VolumeSets(s mac) → aq` | every volume the app set on the pair's transports, in order: a fifth while the user speaks, then the fade back |
| `SetQuirk(s mac, s quirk, v value)` | `off_needs_allow`, `idle_level_only` (with Conversation Awareness on, level 09 alone on every mode change, as AirPods 4), `echo_every`, `echo_delay_ms`, `stale_report_ms` (report the mode an Off replaced, this long after it, ahead of the echo; 0 for none), `silent_handshake`, `refuse_aap` (turn away the next n AAP connections), `handshake_delay_ms` (hold back the handshake's answer), `report_delay_ms` (hold back the battery and what comes with it), `dump_delay_ms` (from the battery to the startup dump and its mode) |
| `Flood(u count)` | that many advertisers no key decrypts and no IRK resolves |
| `Received(s mac) → aay` | every AAP packet the app sent the pair: check `0x34 01` precedes Off |
| `Batteries() → a{sy}` | what GNOME Settings shows, by MAC |

The adapters, and the fake BlueZ behind them, as the desktop's Bluetooth switch, rfkill,
a suspend, a USB dongle or a bluetoothd restart change them:

```sh
scripts/fake-world.sh world SetPowered hci0 false     # the Bluetooth switch off
scripts/fake-world.sh world SetPowered hci0 true
scripts/fake-world.sh world AddAdapter hci1 true      # a dongle plugged in
scripts/fake-world.sh world AddPodsOn hci1 00:00:5E:00:53:02 0x2024 'Dongle pair'
scripts/fake-world.sh world RemoveAdapter hci1
scripts/fake-world.sh world RestartBluez
```

| Method | Does |
|---|---|
| `SetPowered(s adapter, b powered)` | off: discovery there ends, the devices it only heard advertising go, its pairs disconnect (AAP with them) and cannot connect until it is on; providers stay. On again, the pairs stay disconnected until `Connect` |
| `AddAdapter(s adapter, b powered)` | plugs one in at `/org/bluez/<adapter>`; one plugged in before is the same controller, its pairs back, disconnected |
| `RemoveAdapter(s adapter)` | unplugs it: stopped as when off, then its devices, its battery providers and their batteries go |
| `AddPodsOn(s adapter, s mac, q model, s alias)` | `AddPods`, with that adapter |
| `RestartBluez()` | bluetoothd restarting without powering off, as after a crash: it comes back under a new unique name, `org.bluez` changes hands twice, and every client's discovery and battery providers are forgotten; adapters and connections stay. For a `systemctl restart`, which powers off first: `SetPowered` false, `RestartBluez`, `SetPowered` true |

`RestartBluez` is a crash, and a narrow one: only `NameOwnerChanged` tells, and nothing is
announced again. A real bluetoothd also announces every object again with
`InterfacesAdded`, and under systemd answers calls made while it was away, by bus
activation; the fake world's bus activates nothing. A run that passes on `RestartBluez`
proves the `NameOwnerChanged` path only.

The app should log, by adapter, `BLE discovery started` (or `restarted`) once the adapter
is powered again, and `battery provider registered with` after `AddAdapter` or
`RestartBluez` - not after `SetPowered`, which keeps providers. `Batteries` shows the
connected pair again after a `RestartBluez` without reconnecting.

## Drive the UI

```sh
scripts/fake-world.sh app Settle 10000
scripts/fake-world.sh tree | jq '.. | objects | select(.id? == "battery-left") | .label'
scripts/fake-world.sh app SetActive noise-off true
scripts/fake-world.sh app Select device-dropdown 1
scripts/fake-world.sh app Screenshot main-window    # prints the PNG's path: Read it
scripts/fake-world.sh app Notifications             # every notification sent and withdrawn, as JSON
```

`Tree` lists every widget with its `id` (if any), `path`, text and state;
`src/ui/automation.rs` lists the IDs. A press is refused with `Hidden` or `Insensitive`
where a user's click would fail, so Noise Control refuses presses while the device is
on BLE, and so does the `conversation-awareness` switch, until the startup dump has
reported the setting too.

Conversation Awareness: `app SetActive conversation-awareness true` turns it on (`world
Received` shows `0x28 01` sent); then `world Speak 00:00:5E:00:53:01 4000`, and the
Noise Control group shows Transparency from 0.31 s until 0.46 s after the speech, the
app's log the levels at debug (`AAP speech level`).

The volume lowered while the user speaks (#150), with the pair connected and the setting
on:

```sh
scripts/fake-world.sh world SetPlaying 00:00:5E:00:53:01 true
scripts/fake-world.sh world SetVolume 00:00:5E:00:53:01 100
scripts/fake-world.sh world Speak 00:00:5E:00:53:01 4000
scripts/fake-world.sh world VolumeSets 00:00:5E:00:53:01   # 20, then up to 100 in steps
```

The app logs `Lowering the volume of <MAC> to 20` at the first level `01`, and `back at
100` 1.5 s after the speech ends; `RUST_LOG=linuxpods::volume=debug` shows each step. A
`SetVolume` while lowered is the user's: no fade follows, and `VolumeSets` stays at the
one lowering. Without `SetPlaying`, or with `SetCall ... true`, speech sets nothing. A
`HangUp` mid-speech, or quitting the app - Quit, SIGTERM or SIGINT - puts the volume back
at once. A `Disconnect` mid-speech takes the transport along: after `Connect` and
`SetPlaying`, the transport starts at the lowered volume, and the app puts it back
within a second or so. `SetVolumeAnswer 00:00:5E:00:53:01 150 true` first has the pair
answer as real pods may, late and rounded; the app's debug log shows each volume set
and what the transport read then: `set to 30 ..., reading 24`.

Change, settle, then read: `SetBattery`, `Settle`, `tree`.

Low battery notifications: `SetBattery` a part to 20 or below (5 or below is critical),
then `app Notifications` lists the notification sent, by ID `low-battery-<MAC>`. Above 25
again, clear of the low threshold, or charging, it lists the withdrawal - or, while another
part is still low, a send naming only the parts that are. The private session bus has no
notification server, so nothing is shown and GLib warns once; the list records what the app
sent. `Settle` does not wait for a notification: read again until it is there.

The Preferences switch `battery-notifications` turns them off and on, once Preferences is
open (see below): `app SetActive battery-notifications false` withdraws what is on show
and sends nothing more, and `true` shows again what is low. The app saves the switch in
the fake world's own config directory, never the user's -
`$XDG_RUNTIME_DIR/linuxpods-harness/world/home/config/linuxpods/settings.json` - and
`stop` deletes it with the rest of the world, so every `start` begins with it on.

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
- **BLE timing.** Each pod advertises once a second from a resolvable private address of
  its own, made from the pair's IRK, that rotates every 10 s, the left pod's half a
  rotation off the right's; the fake BlueZ drops an address 30 s after it goes quiet.
  Both pods advertise while one is in the case, only the primary while both are out.
  A pod repeats its payload until something changes - an idle pair's for minutes, as on
  hardware (#153) - and the fake BlueZ signals a repeat as `RSSI` alone, as bluetoothd
  does. So the app reads an address's
  first advertisement, in `InterfacesAdded`, and each change, in `PropertiesChanged`
  with `ManufacturerData`, and no repeat: a first reading within about a second of
  start, then one per change. A pod in the closed case stops advertising 4 s after the
  lid closes, and the pods come back under new addresses once it opens, the right one
  after 1 s and the left after 5.5 s. The MAC suffix in the payload is there only
  while both pods are in the case. All of that is as in the #71 captures. Real AirPods
  differ in the rate, about 5 advertisements a second from each pod with bursts of
  30-35 after the lid opens or the pods go back in the case, and in the rotation,
  every 4 to 8 minutes. The fake's rotation stays this fast on purpose, to exercise
  it.
- **Discovery's end takes the pods' devices.** When the last client stops discovering,
  or leaves the bus - the app quitting, with nothing else discovering - the fake BlueZ
  removes every device it only heard advertising, `RSSI` invalidated first, as
  bluetoothd does with the pods' (#153). So an app started next hears each pod at once,
  in an `InterfacesAdded` at its next advertisement. Started while another client
  still discovers, it gets no `InterfacesAdded` for a pod the fake BlueZ kept, and
  nothing of it until its payload or address changes: up to 5 s while both pods
  advertise, 10 s while one does. On hardware that gap was 2.1-4.1 s, with GNOME
  Settings' Bluetooth panel open.
- **AAP timing.** The fake pods answer as AirPods Pro 3 do in the #71 captures, for
  every model: the battery and where the pods are come first, and the noise mode
  only with the startup dump, 200 ms later (1.4-1.5 s on the hardware; set
  `dump_delay_ms` for the real wait). Noise Control is sensitive as soon as the
  reading is on AAP, with no mode selected until the dump. A
  mode change is confirmed by an echo 1 s later; an Off is followed by a stale report
  of the mode it replaced at 0.33 s, which the app should ignore. With Conversation
  Awareness off there is no 0x4B; with it on, level 08 comes at once and 09 after the
  echo, as on a Pro 3 (#150), and 09 after the startup dump. For AirPods 4's answers:
  `SetConversationAwareness` true, `idle_level_only` true, `echo_every` 4,
  `stale_report_ms` 0.
  The handshake is answered at once unless `handshake_delay_ms` says otherwise (about
  1 s on the hardware).
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
