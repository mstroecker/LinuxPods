# Other Bluetooth devices do not reconnect

While LinuxPods scans for AirPods, some other Bluetooth devices may not reconnect on
their own. A mouse or keyboard that you switch off and on may then stay unconnected.
This page says how to tell whether LinuxPods is the cause, how to fix it, and what to
do in the meantime. What it says about the scan was seen with one Bluetooth mouse on
one computer, and how LinuxPods switches has been tried only in its tests; the linked
details say what is still unknown.

## Is LinuxPods the cause?

Open **Preferences** from the LinuxPods main menu:

- If it shows a group named **Other Bluetooth Devices** at the top, LinuxPods is
  scanning in the way that blocks such devices. Read on.
- If it does not, LinuxPods listens without blocking them
  ([details](../app/bluez.md#listening-for-the-pods)). Another app may be scanning
  instead: GNOME Settings' Bluetooth panel does so while it is open
  ([details](../protocol/linux/bluez.md#discovery)).

## Why it happens

To show the battery of AirPods nearby, LinuxPods listens to what they broadcast. Unless
the experimental features of BlueZ, the Linux Bluetooth service, are turned on - they
are off by default - the only way an app can do that is to scan actively. While any app
scans that way, the Linux kernel stops the quiet listening that reconnects such devices,
and its own scan does not hear them asking. They connect as soon as the scan stops.

BlueZ also has a quiet way to listen that leaves these devices free to connect. But
BlueZ counts it as experimental, and keeps it off unless you turn it on.

The details, with their sources:
[Discovery and directed advertising](../protocol/linux/bluez.md#discovery-and-directed-advertising).

## The fix: turn on BlueZ's experimental features

The setting turns on all of BlueZ's experimental features, not only the one LinuxPods
uses. If you rely on other Bluetooth features, check with
[BlueZ](https://github.com/bluez/bluez) first.

1. Open `/etc/bluetooth/main.conf` as root in a text editor, for example:

   ```sh
   sudo nano /etc/bluetooth/main.conf
   ```

2. Find the `[General]` section. Under it, set `Experimental` to `true`. The file
   usually has the line commented out, as `#Experimental = false`. Remove the `#` and
   change `false` to `true`, so that the section reads:

   ```ini
   [General]
   Experimental = true
   ```

3. Save the file, and restart the Bluetooth service:

   ```sh
   sudo systemctl restart bluetooth
   ```

   BlueZ reads the setting only when it starts, so the restart is needed
   ([details](../protocol/linux/bluez.md#discovery-and-directed-advertising)). Your
   Bluetooth devices disconnect
   ([details](../protocol/linux/bluez.md#adapters-and-restarts)); reconnect any that do
   not come back on their own.

LinuxPods notices the restart and switches to the quiet way by itself
([details](../app/bluez.md#listening-for-the-pods)). You do not need to restart
LinuxPods. Once it has switched, the group in Preferences goes away.

If the group is still there after the restart, check that the line is under
`[General]` and has no `#` in front of it. If it is, restart LinuxPods, and please
[open an issue](https://github.com/mstroecker/LinuxPods/issues).

## In the meantime: pause scanning

The group in Preferences has a button, **Pause**, which stops LinuxPods' scan for the
time it shows. A device that is waiting to reconnect connects as soon as the scan
stops. Press it, then switch on or wake the device.

While the scan is paused:

- The group says until when.
- LinuxPods does not hear AirPods nearby. A pair connected to this computer still
  shows its battery.
- Another app's scan still blocks the device. Close GNOME Settings' Bluetooth panel if
  it is open.

Pressing the button again while paused starts the pause over.
