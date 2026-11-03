# Changelog

User-facing changes in each release.

## [0.4.0] - 2026-11-03

Conversation Awareness, with the volume lowered while you speak; low battery
notifications; renaming the AirPods and showing their firmware version; and in-ear
detection that follows the pods live over AAP.

### Features

- **Conversation Awareness:** a switch on the Control page turns it on and off. With it
  on, the pods switch to Transparency while you speak, and the app lowers the pair's own
  volume - its A2DP volume in BlueZ, which PipeWire follows - to a fifth at once, and
  fades it back over 1.5 s once you stop, as an iPhone or a Mac does. Only while audio
  plays to the pair; a volume you change meanwhile is left as you set it, and a lowered
  volume is put back when the pair disconnects or the app quits (#150).
- **Low battery notifications** at 20% and 5%, UPower's defaults, as GNOME gives none for
  Bluetooth batteries (#126). One per pair, once per crossing rather than on every
  reading, and only from a fresh reading. A switch in Preferences turns them off. They
  need the launcher entry that `install.sh` or `make install` writes.
- **Preferences kept across restarts:** the battery notifications switch is saved in
  `~/.config/linuxpods/settings.json` (#146).
- **Rename the AirPods** from the Name row in the new Info group at the bottom of the
  Control page: the name the pods give every host, up to 127 bytes. The window shows it
  at once; GNOME shows it once the pair connects again, unless you gave the pair a name
  in GNOME Settings, and Apple devices keep the name they have (#160).
- **Firmware version** in the Info group, as the iPhone shows it (such as `9A348`),
  selectable for a bug report, and kept after the pair disconnects until the app quits
  (#161).

### Fixes

- **In-ear detection:** while a pair was connected over AAP, in-ear came from its
  advertisements, which change seldom while connected, so the icons lagged or missed
  changes. It now comes from the pods' own reports over AAP, and stays right through a
  role switch between the pods (#142).
- **Connections confirmed with BlueZ:** any program on the system bus could signal a pair
  connected or disconnected, and the app took it as given - it could reconnect AirPods
  you had just disconnected, or withdraw their battery from GNOME Settings. The app now
  asks BlueZ before acting on such a signal (#132).
- **Less work while connected:** with a pod in the case, the other pod's advertisements,
  a few a minute, woke the window, the tray and the app's other tasks although nothing on
  show had changed (#152).

## [0.3.0] - 2026-10-29

Pairs are now recognized over BLE by their own rotating addresses, AAP reconnects on its
own, and the app keeps working through Bluetooth being switched off and on, adapters
coming and going, and bluetoothd restarting.

### Features

- **Identification by address:** an advertisement counts only if its address resolves
  with the pair's IRK and the same pair's key decrypts it. Before, every stored key was
  tried on every advertisement, and among enough foreign AirPods or a BLE spammer a wrong
  key could eventually show random levels for one of your pairs (#134). The IRK is
  fetched on the next AAP connection; until then a pair is identified by its key alone,
  as before. The keys file moves to format version 2, which older versions still read.
- **Color:** the Control page shows the AirPods' color beside the model name, on a line
  of its own.

### Fixes

- **AAP reconnects:** a pair whose AAP link failed or was dropped by the AirPods while
  still connected stayed on BLE until it was disconnected and connected again. Each pair
  now has its link reopened with a backoff, and only while BlueZ reports it connected,
  so AirPods you just disconnected are never reconnected.
- **AAP stuck after a reconnect:** a connect retried too early could leave the AirPods
  holding a stale channel, so the new link's handshake went unanswered and the link
  closed when the AirPods dropped the old one (#136).
- **Bluetooth off and on:** after the first power cycle, suspend, or bluetoothd restart,
  nothing was heard over BLE and GNOME Settings lost the battery until the app was
  restarted. Adapters are now followed as they power off and on, are plugged in or out,
  and bluetoothd restarts (#50). Discovery is retried when bluetoothd refuses it as busy
  instead of silently staying off (#123).
- **No blank moment on connect:** the BLE reading stays on show until the AAP link's
  first battery packet, instead of blanking the window and withdrawing the GNOME Settings
  battery for about a second on every connect (#62).
- **Noise control:** switching modes quickly sends them in order, and the window no longer
  flips back to the old mode for a second after a change (#113). A failed change shows a
  notification. Mode 0x02 is called Noise Cancellation, as Apple calls it.
- **GNOME Settings battery under load:** BlueZ's first read of the battery provider could
  be dropped, leaving no battery in GNOME Settings (#125).
- **Logs:** a link that goes away during setup is no longer reported as an unanswered
  handshake, and AAP packets that may carry keys are redacted in debug logs.

### Removed

- The **Request Keys** button in Preferences → Development: keys are requested on every
  AAP connection, so reconnecting does the same.

## [0.2.0] - 2026-10-25

A reliability release: pairs attach and report battery in more setups, readings stay
current under heavy BLE traffic, and several wrong or stale values are gone.

### Fixes

- **Any Bluetooth adapter:** adapters are found instead of assuming `hci0`, so a machine
  whose adapter has another name (a USB dongle, a re-enumeration after resume) hears its
  AirPods, and a pair on a second adapter gets its battery in GNOME Settings (#51).
- **Battery right after connecting:** a pair that connected while the app ran could stay at
  "Connected • waiting for data" with no battery and Noise Control greyed out. The app now
  waits for the AirPods to answer the handshake before asking for anything.
- **No more false 0%:** a part that is not reporting, such as the case while both pods are
  worn, shows as unknown instead of 0% (#108). GNOME Settings gets a battery only once
  there is a reading, instead of 0% until the first one.
- **Case lid on AirPods Pro 3:** the lid state is now decoded on Pro 3.
- **Model names:** every AirPods model is named from Apple's product ID, and the model and
  colour show when the app starts with the pair already connected.
- **In-ear detection over AAP:** the in-ear icons no longer vanish when a pair connects.
- **Renamed AirPods:** pairs are recognized by their AirPods service, not by "AirPods" in
  their name, so a renamed pair gets AAP and its GNOME Settings battery.
- **Heavy BLE traffic:** advertisement floods no longer stall attaching pairs, delay a
  connecting pair, or leave an older reading on show (#88, #90, #96). The first
  advertisement from each address is read instead of lost.
- **Without a battery provider:** pairs are still attached and get AAP when BlueZ refuses
  the GNOME Settings battery provider.
- **Keys kept safe:** an unreadable keys file is moved aside instead of overwritten, and
  saves are atomic, so stored keys are never lost.
- **Second launch:** starting the app while it already runs no longer starts a second
  backend, tray icon and set of AAP connections.
- **Tray:** the tray resets to "Searching for AirPods…" when no device is left, instead of
  keeping the last reading.
- **Settings that do nothing yet** (Conversation Awareness, Auto-connect, Battery
  notifications) are shown greyed out as "Not available yet".

## [0.1.0] - 2026-10-04

The first release: a native GNOME app for Apple AirPods on Linux, tested on AirPods Pro 3
and AirPods Pro (2nd generation).

### Features

- **Battery over BLE:** left pod, right pod and case read passively from advertisements,
  also while the AirPods are connected to another device. Exact to 1% once the encryption
  key is stored, in ~10% steps without it.
- **Status over BLE:** charging state, in-ear detection, case lid, playback state, and model
  and colour identification behind a randomized BLE address.
- **Over AAP:** live exact battery and charging state, noise control (Transparency,
  Adaptive, ANC, Off), and retrieval of the encryption keys that unlock 1% over BLE.
- **GNOME integration:** battery levels in GNOME Settings → Power, a system tray icon with
  battery and quick actions, and a libadwaita interface.
- **Several pairs at once,** with a switcher. Keys are stored under the XDG data directory
  and reused across sessions.
- **Release downloads:** x86_64 and aarch64 tarballs with an install script, an SBOM,
  checksums and signed SLSA build provenance.
