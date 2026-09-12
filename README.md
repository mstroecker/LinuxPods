# LinuxPods

A native GNOME desktop application for managing Apple AirPods on Linux.

> [!WARNING]
> This project is in very early development. README and documentation may be inaccurate, and many features are not yet implemented.

## Features

What the AirPods can do, grouped by how LinuxPods reaches it.

✅ fully working &nbsp;·&nbsp; 🚧 partial &nbsp;·&nbsp; 📋 planned &nbsp;·&nbsp; ❌ not planned

### Over BLE

Read passively from advertisements. No connection needed, so these keep working while the
AirPods are connected to something else - an iPhone, say. Receive-only: nothing here can
be changed, only observed.

| Feature | Status |
| --- | --- |
| Battery level - left pod, right pod, case | ✅ |
| Exact (1%) battery, once a stored key decrypts the advertisement | ✅ |
| Charging state, per pod and case | ✅ |
| In-ear detection | ✅ |
| Case lid open/closed | ✅ |
| Playback state - idle, music, call, ringing | ✅ |
| Model and colour identification | ✅ |
| Identification behind a randomized BLE MAC | ✅ |

Without a key the levels arrive in ~10% steps, and every 30-60 seconds rather than live.

### Over AAP

Needs an L2CAP connection to this machine, so it covers one pair at a time. This is the
only channel that can *send* commands.

| Feature | Status |
| --- | --- |
| Exact (1%) battery, live | ✅ |
| Charging state | ✅ |
| Encryption key retrieval, which unlocks 1% over BLE | ✅ |
| Noise control - Transparency, Adaptive, ANC, Off | 🚧 |
| Conversation Awareness | 🚧 |
| Device rename | 📋 |
| Press-and-hold gesture configuration | 📋 |
| Personalized and adaptive volume | 📋 |
| Loud sound reduction | 📋 |
| Firmware version | 📋 |

🚧 means the interface is built but the command behind it is still unknown.

### Linux integration

| Feature | Status |
| --- | --- |
| Battery in GNOME Settings → Power, via `org.bluez.BatteryProvider1` | ✅ |
| System tray battery and quick actions (StatusNotifierItem) | ✅ |
| Several pairs tracked at once, with a switcher | ✅ |
| Keys stored under the XDG data directory and reused across sessions | ✅ |
| Native libadwaita interface following the GNOME HIG | ✅ |
| Low battery notifications | 📋 |
| Persisted UI preferences | 📋 |

### Not reachable from Linux

These depend on Apple's audio pipeline, the U1 chip, or iCloud, so they are out of scope
rather than unimplemented.

| Feature | Status |
| --- | --- |
| Ear tip fit test | ❌ |
| Spatial audio and head tracking | ❌ |
| Find My and Precision Finding | ❌ |
| Hearing aid and hearing test | ❌ |
| Automatic device switching between Apple devices | ❌ |
| Announce notifications | ❌ |

## Supported devices

| Device | Status |
| --- | --- |
| AirPods Pro 3 | Tested, fully supported |
| AirPods Pro (2nd generation) | Tested, fully supported |
| Other Apple AirPods | Untested, may work |

## Requirements

GTK4, libadwaita, BlueZ, and Rust 1.85 or newer (edition 2024) to build.

```bash
# Arch Linux
sudo pacman -S gtk4 libadwaita bluez rust

# Ubuntu / Debian
sudo apt install libgtk-4-dev libadwaita-1-dev bluez cargo

# NixOS
nix-shell -p gtk4 libadwaita bluez cargo
```

## Building

```bash
git clone https://github.com/mstroecker/LinuxPods.git
cd LinuxPods
cargo build --release    # or: make build-release
cargo run                # or: make run
```

The first build compiles the GTK4 and libadwaita bindings and takes a couple of minutes.
Later builds are incremental.

## Usage

Launch with `cargo run`, or `./target/release/linuxpods` after a release build.

The **Control** tab shows the three battery levels, charging state and in-ear detection,
along with which source the reading came from. The **Settings** tab lists every known
device with its encryption key status, and is where keys are requested.

To get 1% accuracy over BLE, connect the AirPods to this machine and use
**Settings → Development → Request Keys**. The keys are written to
`~/.local/share/linuxpods/keys.json` and persist, so this is needed only once per device.
From then on the app reads exact levels even when the AirPods are connected elsewhere.

## How it works

A central `Coordinator` merges both sources and broadcasts a snapshot whenever anything
changes. State is keyed by the device's **real** MAC, which is what lets a rotating BLE
address collapse onto a single device.

```
Coordinator (state per device, keyed by real MAC)
    ├─ AAP client ─────────> exact battery for the connected device
    ├─ BLE scanner ────────> advertisements from every other device
    ├─ Per-device choice ──> AAP supersedes BLE only for the device it is connected to
    └─ Broadcasts snapshots to every subscriber:
        ├─ UI ─────────────> battery widgets, device switcher
        ├─ System tray ────> tray menu
        └─ BlueZ provider ─> GNOME Settings battery
```

Each subscriber gets its own channel and receives the current state immediately on
subscribing, so the interface is populated before the first advertisement arrives.

AAP runs over an L2CAP socket on PSM 4097; BLE advertisements are decrypted with AES-128
when a key is stored. Both protocols are documented in full:

- [`docs/ble-proximity-pairing.md`](docs/ble-proximity-pairing.md) - advertisement layout,
  decryption, and how a device is identified behind a randomized address
- [`docs/aap-key-retrieval.md`](docs/aap-key-retrieval.md) - retrieving the IRK and ENC_KEY

## Development

```bash
make test                         # cargo test
make lint                         # cargo clippy --all-targets
cargo fmt

RUST_LOG=linuxpods=debug cargo run   # protocol tracing (make run-trace)
GTK_DEBUG=interactive cargo run      # GTK inspector (make run-debug)
```

Debug logging reports each protocol stage separately:

```
BLE parsable: 5C:4D:3F:B5:41:B6 model=0x2720 payload=25B
BLE decryptable: 5C:4D:3F:B5:41:B6 -> AA:BB:CC:DD:EE:FF (key matched)
BLE AA:BB:CC:DD:EE:FF [decrypted 1%]: left=Some(75) right=Some(73) case=Some(61)
AAP connected to AA:BB:CC:DD:EE:FF (cid 2822, attempt 1)
```

Add `linuxpods=trace` to also see Apple manufacturer data that is not proximity pairing.

Two examples exercise the protocol layers without the interface. `key_request <MAC>`
connects over L2CAP and retrieves the proximity pairing keys; `decrypt_probe` decrypts
captured payloads offline, bypassing validation, which is how the payload layouts were
worked out.

```
src/
├── main.rs        # GTK main loop on the main thread, tokio runtime alongside it
├── lib.rs         # Library target, so the layers can be driven from tests
├── podstate.rs    # Coordinator: merges AAP and BLE, broadcasts snapshots
├── aap/           # Apple Accessory Protocol: client, battery, keys
├── ble/           # Scanner, Apple Continuity parser, AES decryption
├── bluez.rs       # BatteryProvider1 and the device connection watch
├── keystore.rs    # Key storage under XDG data dir
├── indicator.rs   # System tray (StatusNotifierItem)
└── ui.rs          # GTK4/libadwaita interface
```

GTK owns the main thread; tokio carries BLE, AAP and D-Bus work. The two meet over
`async-channel`, since GTK types are `!Send`.

## Contributing

Contributions are welcome. Please:

- Run `cargo fmt` and `cargo clippy --all-targets` before submitting
- Cover protocol parsing and decryption with tests; they need no hardware
- Keep UI changes consistent with the GNOME HIG, and test on multiple window sizes
- Document any protocol discoveries in `docs/`

## Acknowledgments

- [LibrePods](https://github.com/kavishdevar/librepods) - reference for BLE protocol
  reverse engineering and primary pod orientation logic
- [furiousMAC/continuity](https://github.com/furiousMAC/continuity) - Apple Continuity
  protocol documentation
- The BlueZ project - Linux Bluetooth stack and D-Bus API documentation

## License

This project is licensed under the GNU General Public License v3.0 or later
(`GPL-3.0-or-later`). This means:

- You can freely use, modify, and distribute this software
- If you distribute this software, modified or not, you must pass on the source code
  under the same license
- There is no warranty

See the [LICENSE](LICENSE) file for the full license text.
