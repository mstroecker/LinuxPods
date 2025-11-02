# LinuxPods

A modern Linux desktop application for managing Apple AirPods with a native GNOME interface.

> [!WARNING]
> This project is in very early development. README and documentation may be inaccurate, and many features are not yet implemented.

## Features

### ✅ Implemented

- **Real-Time Battery Monitoring**: View live battery levels for left AirPod, right AirPod, and charging case
  - **Automatic Source Selection**: AAP (accurate, 1%) when connected, BLE (1-10%) otherwise
  - **Multi-Device Support**: Track multiple AirPods devices simultaneously
  - **AAP Integration**: Apple Accessory Protocol over L2CAP for precise battery monitoring
  - **BLE Scanning with Optional Decryption**:
    - Unencrypted: ~10% accuracy (no key required)
    - Encrypted: 1% accuracy (requires one-time key retrieval via AAP)
    - Automatic device identification despite BLE MAC randomization (privacy feature)
  - Passive monitoring works while AirPods connected to other devices
  - Charging status indicators (⚡) and in-ear detection (👂)
- **Encryption Key Management**: Settings panel with per-device key status and retrieval
- **System Tray Integration**: Battery levels and quick actions in system tray
- **GNOME Settings Integration**: Battery information appears in GNOME Settings → Power panel (lowest battery level)
- **Native GNOME Design**: Built with libadwaita following GNOME Human Interface Guidelines

### 🚧 Planned

- **Noise Control**: Switch between Transparency, Adaptive, Noise Cancelling, and Off modes (UI ready, protocol TBD)
- **Conversation Awareness**: Toggle to lower media volume when you start speaking (UI ready, protocol TBD)

## Supported Devices

- **Apple AirPods Pro 3**: Tested and fully supported
- **Apple AirPods Pro (2nd Gen)**: Tested and fully supported
- **Other Apple AirPods**: Not tested, may work

## Requirements

### Runtime Dependencies

- GTK4
- libadwaita
- Go 1.25+ (for building)

### Installation

**Arch Linux:**

```bash
sudo pacman -S gtk4 libadwaita go
```

**Ubuntu/Debian:**

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev golang
```

**NixOS:**

```bash
nix-shell -p gtk4 libadwaita go
```

## Building

```bash
# Clone and build
git clone https://github.com/mstroecker/LinuxPods.git
cd linuxpods
go mod download

# Build (using Makefile - recommended)
make build

# Or build directly with go
go build -o linuxpods ./cmd/gui

# Build all debug tools
make tools

# Run
./linuxpods
```

## Usage

### Main Application

Launch the application:

```bash
./linuxpods
```

The application provides:
- **Control Tab**: View all three battery levels, charging status, and in-ear detection
- **Settings Tab**:
  - View all known AirPods devices with encryption key status
  - Request encryption keys for connected devices (enables 1% accuracy BLE monitoring)
  - Shows current connection status and BLE MAC address
- **System Tray**: Quick access to battery info and app controls (right-click tray icon)
- **GNOME Settings**: Battery appears in Settings → Power (shows lowest battery)
- **Automatic Data Source**: Uses AAP (accurate) when connected, BLE (approximate) otherwise
- **Multi-Device Support**: Tracks multiple AirPods devices simultaneously

**How it works:**
1. App starts with BLE scanning for passive battery monitoring (~10% accuracy)
2. When AirPods connect to your computer, app automatically:
   - Detects the connection via BlueZ
   - Establishes AAP connection for accurate battery data (1% accuracy)
   - Switches to using AAP for all battery updates
3. When AirPods disconnect, app falls back to BLE scanning
4. **Optional**: Request encryption keys via Settings → Development to enable 1% accuracy BLE monitoring
   - Keys are automatically saved to `~/.local/share/linuxpods/keys.json` and persist across sessions
   - Allows accurate monitoring even when AirPods connected to other devices

### Debugging Tools (Development/Testing)

LinuxPods includes several debugging tools for testing different components:

**debug_ble** - BLE advertisement scanner with optional decryption:
```bash
# Unencrypted only (~10% accuracy)
go run ./cmd/debug_ble

# With decryption (1% accuracy)
go run ./cmd/debug_ble <ENCRYPTION_KEY>
```
Passively scans for AirPods BLE advertisements and parses Apple Continuity protocol. Works even when AirPods are connected to another device. Supports optional decryption for accurate battery levels.

**debug_aap** - AAP protocol client:
```bash
go run ./cmd/debug_aap <MAC_ADDRESS>
# Example: go run ./cmd/debug_aap 90:62:3F:59:00:2F
```
Tests direct L2CAP connection to AirPods using Apple Accessory Protocol (AAP). Displays raw packets and parsed battery information.

**debug_aap_key_retrieval** - Retrieve encryption keys:
```bash
go run ./cmd/debug_aap_key_retrieval <MAC_ADDRESS>
# Example: go run ./cmd/debug_aap_key_retrieval 90:62:3F:59:00:2F
```
Retrieves proximity pairing encryption keys (IRK and ENC_KEY) from AirPods via AAP connection. The ENC_KEY is used to decrypt BLE advertisements for 1% battery accuracy.

**debug_decrypt_test** - Test BLE parsing and decryption:
```bash
# Unencrypted only
go run ./cmd/debug_decrypt_test

# With decryption
go run ./cmd/debug_decrypt_test <ENCRYPTION_KEY>
```
Tests BLE advertisement parsing and decryption with a hardcoded payload. Useful for verifying encryption keys and understanding the protocol.

**debug_bluez_dbus_discover** - BlueZ device discovery:
```bash
go run ./cmd/debug_bluez_dbus_discover
```
Queries BlueZ D-Bus API to discover paired AirPods and display all device properties, interfaces, and services.

**debug_bluez_dbus_battery** - Battery provider integration test:
```bash
go run ./cmd/debug_bluez_dbus_battery full
```
Tests BlueZ Battery Provider D-Bus API implementation. Verifies batteries appear correctly in GNOME Settings.

## Development

### Project Structure

```
linuxpods/
├── cmd/
│   ├── gui/                        # Main GUI application
│   ├── debug_ble/                  # BLE scanner with optional decryption
│   ├── debug_aap/                  # AAP client debugging tool
│   ├── debug_aap_key_retrieval/    # Retrieve BLE encryption keys
│   ├── debug_decrypt_test/         # Test BLE parsing/decryption
│   ├── debug_bluez_dbus_discover/  # BlueZ device discovery tool
│   └── debug_bluez_dbus_battery/   # BlueZ battery provider test tool
├── internal/
│   ├── podstate/     # AirPods state coordinator
│   ├── ble/          # BLE scanner and proximity pairing parser
│   ├── aap/          # Apple Accessory Protocol (L2CAP) client
│   ├── bluez/        # BlueZ D-Bus battery provider
│   ├── keystore/     # Persistent encryption key storage (XDG Base Directory)
│   ├── ui/           # GTK4/libadwaita UI components
│   ├── indicator/    # System tray indicator
│   └── util/         # Utility functions
├── docs/             # Protocol documentation
│   ├── ble-proximity-pairing.md  # BLE protocol and decryption
│   └── aap-key-retrieval.md      # AAP key retrieval protocol
└── assets/           # PNG images for UI
```

### Technology Stack

This project uses [gotk4](https://github.com/diamondburned/gotk4)
and [gotk4-adwaita](https://github.com/diamondburned/gotk4-adwaita) - Go bindings for GTK4 and libadwaita.

**Why libadwaita?** It provides polished, pre-styled components that match GNOME Settings and follow the GNOME Human
Interface Guidelines.

### Development Setup

```bash
# Install Go dependencies
go get github.com/diamondburned/gotk4/pkg/gtk/v4
go get github.com/diamondburned/gotk4-adwaita/pkg/adw

# Development build with race detector
go build -race -o linuxpods ./cmd/gui

# Run with GTK inspector for debugging
GTK_DEBUG=interactive ./linuxpods
```

### Architecture

#### State Coordination

LinuxPods uses a centralized `PodStateCoordinator` that coordinates all AirPods state data:

```
PodStateCoordinator (central state)
    ├─ AAP Client ───────────> Active connection for accurate battery (when connected)
    ├─ BLE Scanner ──────────> Passive scanning (fallback or when disconnected)
    ├─ Automatic Switching ──> Prefers AAP, falls back to BLE
    ├─ Updates via callbacks:
    │   ├─ UI Window ────────> Updates battery widgets
    │   ├─ System Tray ──────> Updates tray menu
    │   └─ BlueZ Provider ───> Updates GNOME Settings
```

**Two Battery Data Sources (Automatically Selected):**

1. **AAP Client** (Active, 1% accuracy) - **Primary when connected**
   - Apple Accessory Protocol over L2CAP (PSM 4097)
   - Requires AirPods to be connected to Linux via Bluetooth
   - Real-time updates (<1 second)
   - Accurate battery percentages (1% precision)
   - Automatically used when AirPods connect

2. **BLE Scanning** (Passive, 1-10% accuracy) - **Fallback**
   - Scans Apple Continuity proximity pairing advertisements
   - Works while AirPods are connected to other devices (e.g., iPhone)
   - No connection required, updates every 30-60 seconds
   - **Two-tier accuracy system**:
     - **Unencrypted**: ~10% accuracy (no key required)
     - **Encrypted**: 1% accuracy (requires one-time key retrieval via AAP)
   - **BLE MAC Randomization**: AirPods randomize their BLE MAC address for privacy
     - App identifies devices by trying stored encryption keys until validation succeeds
     - Uses magic bytes (byte 0 upper nibble = 0x0, byte 4 = 0x2D) to validate decryption
     - Encryption keys stored by real MAC address (from AAP connection)
   - See `docs/ble-proximity-pairing.md` and `docs/aap-key-retrieval.md` for protocol details

#### BlueZ Integration

LinuxPods implements BlueZ's Battery Provider D-Bus API (`org.bluez.BatteryProvider1`):

- Battery appears in GNOME Settings → Power panel
- Shows **lowest battery level** (most useful for knowing when to charge)
- Proper D-Bus ObjectManager pattern with InterfacesAdded/Removed signals

**Note**: BlueZ displays one battery per device. Use LinuxPods app to view all three batteries separately.

## Acknowledgments

This project builds on research and implementations from:

- **[LibrePods](https://github.com/kavishdevar/librepods)** - Reference for BLE protocol reverse engineering and primary pod orientation logic
- **[furiousMAC/continuity](https://github.com/furiousMAC/continuity)** - Apple Continuity protocol documentation
- **BlueZ Project** - Linux Bluetooth stack and D-Bus API documentation

## Contributing

Contributions are welcome! Please:

- Follow Go conventions and run `go fmt`
- Keep UI changes consistent with GNOME HIG
- Test on multiple window sizes
- Document any protocol discoveries in `docs/`

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0). This means:

- You can freely use, modify, and distribute this software
- If you modify and distribute this software, you must share your source code under the same license
- If you run a modified version as a network service, you must make your source code available to users

See the [LICENSE](LICENSE) file for the full license text.

## Status

### ✅ Completed

- [x] BlueZ Battery Provider D-Bus integration
- [x] Battery information in GNOME Settings (lowest battery)
- [x] Real-time battery monitoring via BLE scanning
- [x] **BLE advertisement decryption for 1% battery accuracy**
- [x] **AAP-based encryption key retrieval**
- [x] **Multi-device support** (track multiple AirPods simultaneously)
- [x] **BLE MAC randomization handling** (automatic device identification)
- [x] **Encryption key management UI** (Settings panel with per-device key status)
- [x] Apple Accessory Protocol (AAP) client implementation
- [x] **AAP integration into main app with automatic switching**
- [x] **Accurate battery monitoring when AirPods connected**
- [x] System tray icon with battery display
- [x] Charging status indicators
- [x] In-ear detection (via BLE)
- [x] Centralized AirPods state coordination
- [x] Comprehensive BLE protocol documentation (unencrypted + encrypted)
- [x] **Persistent encryption key storage** (XDG Base Directory: `~/.local/share/linuxpods/`)

### 🚧 In Progress / Planned

- [ ] Functional noise control mode switching (UI ready, AAP commands TBD)
- [ ] Functional conversation awareness toggle (UI ready, AAP commands TBD)
- [ ] Configuration storage for app settings (XDG Base Directory)
- [ ] Persist UI preferences across sessions
- [ ] Battery level notifications (low battery warnings)
- [ ] Support for other Apple audio devices (AirPods Max, Beats, etc.)
- [ ] Connection status indicator in UI
