# Changelog

User-facing changes in each release.

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
