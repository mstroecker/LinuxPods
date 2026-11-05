# The desktop

What the desktop around BlueZ does that a client has to know: PipeWire following the
transport's volume, MPRIS, the path a press on a pod takes to a player, and UPower's
warning level for a Bluetooth battery. Checked on hardware where a run below says so;
MPRIS is from its specification, UPower also from its source, and the key path is only
as understood, not traced.

The app acts on these in `volume`, `media` and `notify`:
[app/volume.md](../../app/volume.md), [app/media-pause.md](../../app/media-pause.md),
[app/notifications.md](../../app/notifications.md).

## The volume

**PipeWire's sink follows the transport's `Volume`.** Set on the pair's A2DP transport,
`Volume` moved the pair's sink within 0.5 s (Pro 3, 2026-11-01, #150; run below).

The transport's `Volume`: [bluez.md](bluez.md#mediatransport1volume).

## MPRIS

Media players are controlled over MPRIS, on the session bus
([MPRIS D-Bus Interface Specification](https://specifications.freedesktop.org/mpris-spec/latest/)
2.2, cited in #167):

- **Players** own names starting `org.mpris.MediaPlayer2.`, and serve
  `org.mpris.MediaPlayer2.Player` at `/org/mpris/MediaPlayer2`.
- **`PlaybackStatus`** reads `Playing`, `Paused` or `Stopped`.
- **`Pause` and `Play`** each go one way only: `Pause` on a paused player and `Play` on
  a playing one change nothing.
- **`PlayPause` is a toggle**: sent to a player that is not playing, it starts it.

## A press on a pod

As understood - not traced here (#167):

1. A press on a pod travels as an AVRCP play/pause passthrough.
2. bluetoothd turns it into a key press on an input device of its own,
   `<name> (AVRCP)`, through uinput.
3. GNOME's media keys service (`org.gnome.SettingsDaemon.MediaKeys`) hands the key to the
   one player it takes as current, over MPRIS.

A key is a toggle, as `PlayPause` is. Synthesizing a key of one's own needs write access
to `/dev/uinput`, which takes root or a udev rule, and the shell then picks the player
that gets it (#167).

Why the app does not go this way to pause the media:
[app/media-pause.md](../../app/media-pause.md).

## UPower

**GNOME never warns of a low Bluetooth battery.** Seen on a Pro 3 (2026-10-26, #126, run
below), and true of every battery that bluetoothd reports as `Battery1`:

- **UPower gives a warning level only to a device that is discharging**
  (`up_daemon_compute_warning_level` in UPower 1.91.5's `src/up-daemon.c`; #126).
- **Its BlueZ backend reads `Percentage` alone** (UPower 1.91.5,
  `src/linux/up-device-bluez.c`; #126). The device's state stays unknown, so its
  `WarningLevel` stays none.
- **`Battery1` carries no state** in BlueZ 5.87. BlueZ master adds an experimental
  `Charging`, which UPower does not read (#126).
- **With `WarningLevel` none, `gsd-power` sends nothing** (#126).
- **UPower's default thresholds** are 20% (low) and 5% (critical): `PercentageLow` and
  `PercentageCritical` in UPower 1.91.5's `etc/UPower.conf` (#126).

## Runs

**The volume: Pro 3, 2026-11-01 (#150).** The transport's `Volume` was set from 32 to 20
and back with `busctl`, which was not the transport's owner. PipeWire's sink followed
within 0.5 s: 25% to 16% to 25%.

**UPower: Pro 3, connected, 2026-10-26 (#126).** A battery provider reported the pair to
bluetoothd at 9%, then 4%. UPower 1.91.5 listed it as headphones at that level, with
state unknown and `WarningLevel` none, and `gsd-power` sent nothing.

## Open questions

- A press on a pod: confirm the path above - `evtest` shows the key, and the MPRIS
  player changes state (#167).
- GNOME's volume slider, which follows the sink, was not watched in the volume run
  (#150).
