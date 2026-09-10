# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LinuxPods is a native GNOME desktop application for managing Apple AirPods on Linux. It provides real-time battery
monitoring, system tray integration, and GNOME Settings integration using a libadwaita-based UI that follows GNOME
Human Interface Guidelines.

**Technology Stack:**
- **Language:** Rust (edition 2024, rust-version 1.85+)
- **UI Framework:** GTK4 via [gtk4-rs](https://github.com/gtk-rs/gtk4-rs)
- **UI Components:** libadwaita via [libadwaita-rs](https://gitlab.gnome.org/World/Rust/libadwaita-rs)
- **D-Bus:** [zbus](https://github.com/dbus2/zbus) for BlueZ discovery and the battery provider
- **L2CAP:** [bluer](https://github.com/bluez/bluer) (`default-features = false`, so no second D-Bus stack)
- **Async:** tokio for protocol work, `async-channel` to reach the GTK main context
- **Target Platform:** Linux (GNOME desktop environment)

## Build and Development Commands

```bash
make            # fmt + build
make build      # cargo build
make run        # cargo run
make run-debug  # GTK inspector + debug logging
make run-trace  # RUST_LOG=linuxpods=debug
make test       # cargo test
make lint       # cargo clippy --all-targets
make clean      # cargo clean

cargo run --example key_request <MAC>   # AAP connect + key retrieval
cargo run --example decrypt_probe       # offline decryption of captured payloads
```

The first build compiles the GTK4/libadwaita bindings (a couple of minutes). Later builds are incremental.

**Verify the binary is fresh before testing against hardware.** `cargo test` builds test harnesses, not
`target/debug/linuxpods`; running a stale binary has wasted debugging time more than once.

## Architecture

```
src/
├── main.rs        # GTK main loop on the main thread; tokio runtime alongside it
├── lib.rs         # Library target so layers can be driven from tests and examples
├── podstate.rs    # Coordinator: merges AAP and BLE, broadcasts snapshots
├── aap/           # Apple Accessory Protocol over L2CAP (PSM 4097)
├── ble/           # Scanner, Apple Continuity parser, AES decryption
├── bluez.rs       # org.bluez.BatteryProvider1 + device connection watch
├── keystore.rs    # Encryption keys at ~/.local/share/linuxpods/keys.json (0600)
├── indicator.rs   # System tray via StatusNotifierItem (ksni)
└── ui.rs          # GTK4/libadwaita interface
```

### Threading model

GTK owns the main thread; tokio carries BLE, AAP and D-Bus work. GTK types are `!Send`, so the two meet over
`async-channel` receivers consumed with `glib::spawn_future_local`. Never touch a widget from a tokio task.

### State coordination

`Coordinator` merges two sources **per device**:

- **AAP** - exact battery, requires an L2CAP connection
- **BLE** - advertisements, 10% steps, or exact when a stored key decrypts them

An AAP connection supersedes BLE **only for the connected device** (`supersedes_ble`). A global pause would blank out
every other pair of AirPods. The check runs *after* decryption, because an advertisement carries only a random MAC
until it is decrypted.

`subscribe()` returns a channel per consumer and delivers the current state immediately. Both matter:

- A single shared `async_channel::Receiver` is MPMC - each snapshot reaches exactly *one* consumer, so the UI, tray
  and battery provider would compete for updates instead of all receiving them.
- Without an immediate snapshot, a subscriber that starts before the first advertisement renders nothing, and the
  window comes up blank whenever no device is connected or in range.

Both `connect_aap` and `disconnect_aap` broadcast. Disconnect also drops the device's AAP state, otherwise the UI
keeps showing a stale exact-looking reading for a device that has gone.

### MAC randomization

Apple rotates the advertised BLE MAC continuously while disconnected - four distinct addresses for one device inside
45 seconds is normal. Decryption is what maps a rotating address back to the real one, so **state is keyed by the real
MAC**. Devices that cannot be identified still need `DEVICE_TTL` pruning, since their addresses cannot be collapsed.

## Protocol Notes

### BLE decryption and validation

Validate a decrypted proximity payload by **MAC suffix**: bytes 7-9 hold the last three bytes of the device's real
MAC. A match both validates the decryption and identifies the device.

A connected device zeroes that field, so `00 00 00` is accepted too. Identification still holds: it comes from which
key decrypted the payload, not from reading the suffix. The union costs one bit (2^-23 per wrong key, from 2^-24).

⚠️ Do **not** validate with magic bytes. The older check (byte 0 upper nibble `0x0`, byte 4 `0x2D`) holds on neither
AirPods Pro 3 (0x2720) nor Pro Gen 2 (0x2420) - both report byte 4 = `0x1D` - and rejects correct decryptions, which
silently disables 1% accuracy. See `docs/ble-proximity-pairing.md`.

AAP packets carry battery only. Model, colour and orientation are carried forward from BLE state for the same device.

### AAP over L2CAP

`SeqPacket::connect` returns almost immediately without waiting for the BR/EDR ACL link. When that link is not yet up
it still reports `Ok`, but no channel was established and every send fails with `ENOTCONN` - and the socket stays dead
even after its `cid` later becomes nonzero. **A zero `cid` immediately after connect means the socket is unusable**;
discard it and retry with a fresh one.

Never hold the `aap_client` mutex across `read_packet().await`. The read parks until a packet arrives, and anything
else wanting the connection then waits forever - this deadlocked the Request Keys button.

## Code Patterns and Best Practices

- **Keep protocol parsing pure and tested.** `aap::battery`, `aap::keys`, `ble::parser` and `ble::decrypt` take bytes
  and return values; they need no hardware and carry the bulk of the test suite.
- **Use synthetic test vectors.** Never commit a real device key. Construct a plaintext, encrypt it in the test, and
  assert the round trip.
- **`Option<u8>` for unknown battery**, never a sentinel. Levels above 100 mean unavailable.
- **Log the stages separately.** `BLE parsable:` in the scanner covers every advertisement that parses, including ones
  the coordinator later drops; `BLE decryptable:` reports identification. Noisy non-proximity Apple data goes to
  `trace`, not `debug`.
- **Log text describing control flow goes stale like comments do**, and is more visible because it is read at runtime.

### UI

- Widget updates arrive only from the snapshot stream; there is no other path into the UI.
- Read a selection **before** mutating the widget: splicing a `StringList` makes `GtkDropDown` emit `selected-notify`,
  which will otherwise clobber the user's choice.
- Noise Control and Features are commands, so they are insensitive unless the device is on AAP. BLE is receive-only.
- Assets are resolved at runtime from `CARGO_MANIFEST_DIR`; a missing file fails silently, so a test asserts they exist.

## Documentation

- `docs/ble-proximity-pairing.md` - Apple Continuity proximity pairing, decrypted layout, MAC-suffix validation
- `docs/aap-key-retrieval.md` - retrieving IRK and ENC_KEY over AAP
