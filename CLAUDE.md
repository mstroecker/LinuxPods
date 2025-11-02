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
│   ├── keystore/     # Persistent encryption key storage (XDG Base Directory)
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

2. **BLE Scanner** (internal/ble/) - Passive monitoring, 1-10% accuracy
   - Scans Apple Continuity proximity pairing advertisements
   - Works when AirPods connected to other devices
   - Fallback when no active connection available

**PodStateCoordinator** automatically switches between sources and notifies:
- UI window (internal/ui/) - Updates battery widgets
- System tray (internal/indicator/) - Updates tray menu
- BlueZ provider (internal/bluez/) - Updates GNOME Settings

### Encryption Key Storage
- **internal/keystore/**: Persistent storage for BLE encryption keys
  - Follows XDG Base Directory specification: `~/.local/share/linuxpods/keys.json`
  - Keys stored as JSON with base64 encoding, indexed by MAC address
  - Automatically loaded on startup and saved when new keys are received
  - In-memory cache with explicit Load/Save operations
  - File permissions: 0600 (owner read/write only)
  - Keys enable 1% accuracy BLE monitoring even when AirPods connected to other devices

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
  - Persistent storage in internal/keystore/
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
- Retrieving encryption keys? Use `go run ./cmd/debug_aap_key_retrieval <MAC_ADDRESS>`

## Code Patterns and Best Practices

### Helper Functions
- Extract repetitive code into helper functions with clear names
- Use multiple return values instead of pointer parameters: `func helper() (*int, bool)` not `func helper(out **int, flag *bool)`
- Example: `getBatteryFromAAP()` in coordinator.go, `updateBatteryMenuItem()` in indicator.go

### Constants and Immutability
- Use **arrays** for fixed-size data constants (e.g., protocol packets): `var packet = [16]byte{...}`
- Arrays are better than slices for constants: fixed size, stack-allocatable, clearer intent
- Slice arrays when passing to functions that expect `[]byte`: `sendPacket(packet[:], "name")`
- Go doesn't support `const` for composite types; arrays are as close as we can get

### Protocol Validation
- **BLE Decryption**: `DecryptProximityPayload()` validates decrypted data using magic bytes:
  - Byte 0 upper nibble must be `0x0`
  - Byte 4 must be `0x2D`
  - This helps identify correct decryption (wrong keys produce garbage but AES always "succeeds")
- Always validate protocol data before use; return errors for invalid data

### File Organization
- Group related functionality (e.g., `stringer.go` for String() methods separate from core parsing)
- Keep debug-only functions in debug tools, not in production packages
- Example: Key retrieval helpers moved from `internal/aap/client.go` to `cmd/debug_aap_key_retrieval/main.go`

### Error Handling
- Use errors to signal validation failures (e.g., decryption with wrong key)
- Let callers handle errors; don't silently ignore invalid data
- Log significant events (device identification, connection state changes) but not routine operations

### Multi-Device Support
- State is managed per device using MAC address as key: `map[string]*PodState`
- BLE advertisements use randomized MAC addresses (privacy feature)
- Identify devices by trying all stored encryption keys until validation succeeds
- Store encryption keys by real MAC address (from AAP connection), not BLE MAC

### Encryption Key Persistence (Implemented)
The keystore package (`internal/keystore/`) provides persistent storage for BLE encryption keys:

**Implementation:**
- **Storage location**: `~/.local/share/linuxpods/keys.json` (XDG Base Directory spec)
- **Format**: JSON with base64-encoded keys, indexed by MAC address
- **Permissions**: File created with 0600 (owner read/write only)
- **API Pattern**: Separate Load/Save operations with in-memory cache
  ```go
  ks, _ := keystore.New()           // Create keystore
  keys, _ := ks.Load()              // Load from disk
  ks.Set(macAddr, key)              // Update in memory
  ks.Save()                         // Persist to disk
  key, exists := ks.Get(macAddr)    // Retrieve from cache
  ```

**Integration with PodStateCoordinator:**
- Keys loaded on startup and populate the `encryptionKeys` map
- When AAP receives encryption keys, they're automatically saved to disk
- Enables BLE decryption (1% accuracy) immediately on subsequent app launches
- No re-connection via AAP needed to decrypt BLE advertisements

**Storage format example:**
```json
{
  "version": 1,
  "keys": {
    "AA:BB:CC:DD:EE:FF": "AQIDBAUGBwgJCgsMDQ4PEA=="
  }
}
```

### Future: Additional Configuration Storage
When implementing UI preferences and app settings:
- Use **XDG Base Directory** specification:
  - `~/.config/linuxpods/` for user configuration files (TOML/JSON)
  - `~/.local/share/linuxpods/` for application data (already used for encryption keys)
  - `~/.cache/linuxpods/` for temporary/cache data
- Consider **GNOME Keyring** for future sensitive data if needed:
  - Library: `github.com/zalando/go-keyring`
  - Encryption keys currently use filesystem storage with restrictive permissions (0600)
