# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LinuxPods is a native GNOME desktop application for managing Apple AirPods on Linux. It provides real-time battery monitoring, system tray integration, and GNOME Settings integration using a libadwaita-based UI that follows GNOME Human Interface Guidelines.

**Technology Stack:**
- **Language:** Go 1.25+
- **UI Framework:** GTK4 via [gotk4](https://github.com/diamondburned/gotk4)
- **UI Components:** libadwaita via [gotk4-adwaita](https://github.com/diamondburned/gotk4-adwaita)
- **Bluetooth:** BlueZ D-Bus API for device management and battery provider
- **Target Platform:** Linux (GNOME desktop environment)

## Build and Development Commands

### Using Makefile (Recommended)
```bash
# Format code and build (default target)
make

# Build the main GUI application
make build

# Build with race detector (for development)
make build-race

# Build all debugging tools
make tools

# Run the application
make run

# Run with GTK inspector for UI debugging
make run-debug

# Format code
make fmt

# Clean build artifacts
make clean
```

### Direct Go Commands
```bash
# Standard build
go build -o linuxpods ./cmd/gui

# Development build with race detector
go build -race -o linuxpods ./cmd/gui

# Format code (must be run before committing)
go fmt ./...

# Run with GTK inspector for UI debugging
GTK_DEBUG=interactive ./linuxpods
```

## Architecture

### Project Structure
```
linuxpods/
├── cmd/
│   ├── gui/                        # Main GUI application
│   ├── debug_ble/                  # BLE scanner debugging tool
│   ├── debug_aap/                  # AAP client debugging tool
│   ├── debug_bluez_dbus_discover/  # BlueZ device discovery tool
│   └── debug_bluez_dbus_battery/   # BlueZ battery provider test tool
├── internal/
│   ├── podstate/     # AirPods state coordinator
│   ├── ble/          # BLE scanner for Apple Continuity advertisements
│   ├── aap/          # Apple Accessory Protocol (L2CAP) client
│   ├── bluez/        # BlueZ D-Bus battery provider
│   ├── ui/           # GTK4/libadwaita UI components
│   ├── indicator/    # System tray indicator
│   └── util/         # Utility functions
├── docs/             # Protocol documentation
├── assets/           # PNG images for UI
└── Makefile          # Build targets
```

### Application Entry Point
- **cmd/gui/main.go**: Main entry point that creates the Adwaita application and initializes the state coordinator, UI, and system tray

### State Coordination System
The application uses a centralized `PodStateCoordinator` (internal/podstate/) that coordinates all AirPods state data sources:

**Two Data Sources (Automatically Selected):**
1. **AAP Client** (internal/aap/) - Active connection, 1% accuracy
   - Apple Accessory Protocol over L2CAP (PSM 4097)
   - Used when AirPods are connected to Linux device
   - Real-time updates with accurate battery levels

2. **BLE Scanner** (internal/ble/) - Passive monitoring, 5-10% accuracy
   - Scans Apple Continuity proximity pairing advertisements
   - Works when AirPods connected to other devices
   - Fallback when no active connection available

**PodStateCoordinator** automatically switches between sources and notifies:
- UI window (internal/ui/) - Updates battery widgets
- System tray (internal/indicator/) - Updates tray menu
- BlueZ provider (internal/bluez/) - Updates GNOME Settings

### BlueZ Integration
- **internal/bluez/battery_provider.go**: Implements org.bluez.BatteryProvider1 D-Bus API
- Registers custom battery provider with BlueZ
- Battery appears in GNOME Settings → Power panel
- Displays lowest battery level (most useful for charging decisions)

### UI Layer
- **internal/ui/window.go**: Contains all UI construction logic
  - `Activate()`: Creates the main application window
  - `setupUI()`: Builds the complete UI hierarchy including:
    - Battery level displays for left AirPod, right AirPod, and case
    - Charging status indicators (⚡) and in-ear detection (👂)
    - Noise control preference group with radio buttons (UI only, protocol TBD)
  - Uses AdwPreferencesGroup and AdwActionRow for settings-style UI
  - Loads PNG assets from assets/ directory for AirPod visualizations

### Assets
- **assets/**: Contains PNG images for left AirPod, right AirPod, and charging case displayed in the battery monitoring section

### Debugging Tools
All debugging tools are in cmd/debug_* directories and include comprehensive documentation:
- **debug_ble**: Passively scan for AirPods BLE advertisements
- **debug_aap**: Test AAP protocol connection and packet parsing
- **debug_bluez_dbus_discover**: Query BlueZ D-Bus for paired devices
- **debug_bluez_dbus_battery**: Test battery provider D-Bus integration

## Important Development Notes

### GTK4/libadwaita Development
- The project uses Go bindings for GTK4, not native GTK - all UI code is written in Go
- libadwaita provides GNOME-styled components that automatically match system themes
- UI hierarchy: AdwApplicationWindow → Box containers → PreferencesGroup → ActionRow components
- Image assets must be accessible at runtime in the assets/ directory relative to the executable

### Signal Handling
- GTK widgets use the `Connect()` method to attach event handlers
- Radio button groups are created by calling `SetGroup()` on subsequent buttons with the first button as argument
- Check button state changes trigger the "toggled" signal

### UI Debugging
- Use `GTK_DEBUG=interactive` (or `make run-debug`) to launch the GTK Inspector for runtime UI inspection
- The inspector allows viewing the widget hierarchy, CSS, and properties

### Bluetooth/Protocol Development
- **BLE Protocol**: See docs/ble-proximity-pairing.md for Apple Continuity protocol documentation
- **AAP Protocol**: Apple Accessory Protocol uses L2CAP PSM 4097 for direct communication
- **BlueZ D-Bus**: Use debug_bluez_dbus_discover to inspect device properties and interfaces
- All protocol implementations are in internal/ packages with corresponding debug tools

### Code Organization
- **internal/**: All application-specific packages (not for external consumption)
  - UI code in internal/ui/
  - State coordination in internal/podstate/
  - Protocol implementations in internal/aap/, internal/ble/, internal/bluez/
  - System integration in internal/indicator/, internal/util/
- **cmd/**: Command entry points - all main packages
  - cmd/gui/ is the main application
  - cmd/debug_*/ are debugging/testing tools
- **docs/**: Protocol documentation and reverse engineering notes
- **assets/**: UI resources (images for AirPods visualizations)

### Debugging Tools Usage
When working on specific components, use the corresponding debug tool:
- Developing BLE parsing? Use `go run ./cmd/debug_ble`
- Testing AAP connection? Use `go run ./cmd/debug_aap <MAC_ADDRESS>`
- Debugging D-Bus integration? Use `go run ./cmd/debug_bluez_dbus_battery full`
- Finding device paths? Use `go run ./cmd/debug_bluez_dbus_discover`
