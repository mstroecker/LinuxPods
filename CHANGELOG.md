# Changelog

User-facing changes in each release.

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
