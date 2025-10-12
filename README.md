# LinuxPods

A modern Linux desktop application for managing Apple AirPods with a native GNOME interface.

## Features

### ✅ Implemented

- **Real-Time Battery Monitoring**: View live battery levels for left AirPod, right AirPod, and charging case
  - BLE scanning for passive monitoring (works while AirPods connected to other devices)
  - AAP (Apple Accessory Protocol) client for accurate, real-time data
  - Charging status indicators (⚡) and in-ear detection (👂)
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
go build -o linuxpods ./cmd/gui

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
- **Main Window**: View all three battery levels, charging status, and in-ear detection
- **System Tray**: Quick access to battery info and app controls (right-click tray icon)
- **GNOME Settings**: Battery appears in Settings → Power (shows lowest battery)

### CLI Tools (Development/Testing)

**BLE Scanner** - Test BLE advertisement parsing:
```bash
go run ./cmd/ble_scan
```

**AAP Client** - Test direct AirPods connection:
```bash
go run ./cmd/aap <bluetooth-device-path>
# Example: go run ./cmd/aap /org/bluez/hci0/dev_90_62_3F_59_00_2F
```

**BlueZ Debug** - Test BlueZ D-Bus integration:
```bash
go run ./cmd/debug
```

## Development

### Project Structure

```
linuxpods/
├── cmd/
│   ├── gui/          # Main GUI application
│   ├── aap/          # AAP client CLI tool (testing)
│   ├── ble_scan/     # BLE scanner CLI tool (testing)
│   └── debug/        # BlueZ D-Bus debugging tools
├── internal/
│   ├── battery/      # Central battery state manager
│   ├── ble/          # BLE scanner for Apple Continuity advertisements
│   ├── aap/          # Apple Accessory Protocol (L2CAP) client
│   ├── bluez/        # BlueZ D-Bus battery provider
│   ├── ui/           # GTK4/libadwaita UI components
│   └── indicator/    # System tray indicator
├── docs/             # Protocol documentation
│   └── ble-proximity-pairing.md  # BLE protocol reverse engineering
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

#### Battery Monitoring

LinuxPods uses a centralized `BatteryManager` that coordinates all battery-related functionality:

```
BatteryManager (central state)
    ├─ BLE Scanner ──────────> Scans for Apple Continuity advertisements
    ├─ Updates via callbacks:
    │   ├─ UI Window ────────> Updates battery widgets
    │   ├─ System Tray ──────> Updates tray menu
    │   └─ BlueZ Provider ───> Updates GNOME Settings
```

**Two Battery Data Sources:**

1. **BLE Scanning** (Passive, ~5-10% accuracy)
   - Scans Apple Continuity proximity pairing advertisements
   - Works while AirPods are connected to other devices (e.g., iPhone)
   - No connection required, updates every 3-5 seconds
   - See `docs/ble-proximity-pairing.md` for protocol details

2. **AAP Client** (Active, 1% accuracy)
   - Apple Accessory Protocol over L2CAP (PSM 4097)
   - Requires AirPods to be connected to Linux
   - Real-time updates (<1 second)
   - Currently used for testing, will be integrated later

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
- [x] Apple Accessory Protocol (AAP) client implementation
- [x] System tray icon with battery display
- [x] Charging status indicators
- [x] In-ear detection (via BLE)
- [x] Centralized battery state management
- [x] Comprehensive BLE protocol documentation

### 🚧 In Progress / Planned

- [ ] Functional noise control mode switching (UI ready, AAP commands TBD)
- [ ] Functional conversation awareness toggle (UI ready, AAP commands TBD)
- [ ] Integrate AAP client into main app (currently separate CLI tool)
- [ ] Persist settings across sessions
- [ ] Battery level notifications (low battery warnings)
- [ ] Support for other Apple audio devices (AirPods Max, Beats, etc.)
- [ ] Automatic reconnection handling
- [ ] Connection status in UI
