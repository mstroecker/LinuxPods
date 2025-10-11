# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LinuxPods is a native GNOME desktop application for managing Apple AirPods on Linux. It provides battery monitoring and noise control features using a libadwaita-based UI that follows GNOME Human Interface Guidelines.

**Technology Stack:**
- **Language:** Go 1.25+
- **UI Framework:** GTK4 via [gotk4](https://github.com/diamondburned/gotk4)
- **UI Components:** libadwaita via [gotk4-adwaita](https://github.com/diamondburned/gotk4-adwaita)
- **Target Platform:** Linux (GNOME desktop environment)

## Build and Development Commands

### Building
```bash
# Standard build
go build -o linuxpods ./cmd/gui

# Development build with race detector
go build -race -o linuxpods ./cmd/gui

# Download dependencies
go mod download
```

### Running
```bash
# Normal run
./linuxpods

# Run with GTK inspector for UI debugging
GTK_DEBUG=interactive ./linuxpods
```

### Code Quality
```bash
# Format code (must be run before committing)
go fmt ./...

# Run with race detector
go run -race ./cmd/gui
```

## Architecture

### Application Entry Point
- **cmd/gui/gui.go**: Main entry point that creates the Adwaita application with app ID "com.example.myapp" and delegates UI setup to the internal/ui package

### UI Layer
- **internal/ui/window.go**: Contains all UI construction logic
  - `Activate()`: Creates the main application window
  - `setupUI()`: Builds the complete UI hierarchy including:
    - Battery level displays for left AirPod, right AirPod, and case
    - Noise control preference group with radio buttons for Transparency, Adaptive, Noise Cancelling, and Off modes
  - Uses AdwPreferencesGroup and AdwActionRow for settings-style UI
  - Loads PNG assets from assets/ directory for AirPod visualizations

### Assets
- **assets/**: Contains PNG images for left AirPod, right AirPod, and charging case displayed in the battery monitoring section

### Future Architecture
- **pkg/**: Reserved for public libraries (currently empty)
- Bluetooth integration with BlueZ is planned but not yet implemented
- Currently displays mock battery data (hardcoded to 75%)

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
- Use `GTK_DEBUG=interactive` to launch the GTK Inspector for runtime UI inspection
- The inspector allows viewing the widget hierarchy, CSS, and properties

### Code Organization
- UI code should remain in internal/ui/ - this package is not intended for external consumption
- Future Bluetooth/AirPods communication logic should go in internal/ packages
- Only stable, reusable libraries should be placed in pkg/ for external use
