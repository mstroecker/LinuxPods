# LinuxPods

A modern Linux desktop application for managing Apple AirPods with a native GNOME interface.

## Features

- **Battery Monitoring**: View battery levels for left AirPod, right AirPod, and charging case in the app
- **BlueZ's Battery Provider D-Bus Integration**: Battery information appears in e.g., GNOME Settings → Power panel (
  lowest battery level)
- **Noise Control**: Switch between Transparency, Adaptive, Noise Cancelling, and Off modes
- **Conversation Awareness**: Toggle to lower media volume when you start speaking
- **Native GNOME Integration**: Built with libadwaita for a polished, native look

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

## Development

### Project Structure

```
linuxpods/
|-- cmd/gui/          # Application entry point
|-- internal/ui/      # UI components
|-- internal/bluez/   # BlueZ D-Bus battery provider integration
|-- assets/           # Image assets
|-- pkg/              # Public libraries (future)
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

### BlueZ Integration

LinuxPods integrates with BlueZ's Battery Provider D-Bus API to expose battery information system-wide:

- Battery levels appear in GNOME Settings
- Uses org.bluez.BatteryProvider1 interface
- Implements proper D-Bus ObjectManager pattern

**Note**: BlueZ only displays one battery per Bluetooth device in system settings. Use the LinuxPods app to view all
three battery levels separately.

## Contributing

Contributions are welcome! Please:

- Follow Go conventions and run `go fmt`
- Keep UI changes consistent with GNOME HIG
- Test on multiple window sizes

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0). This means:

- You can freely use, modify, and distribute this software
- If you modify and distribute this software, you must share your source code under the same license
- If you run a modified version as a network service, you must make your source code available to users

See the [LICENSE](LICENSE) file for the full license text.

## TODO

- [x] Implement BlueZ Battery Provider integration
- [x] Battery information in GNOME Settings
- [ ] Implement real AirPods Bluetooth communication (decode Apple proprietary protocol)
- [ ] Live battery level updates from connected AirPods
- [ ] Functional noise control mode switching
- [ ] Functional conversation awareness toggle
- [ ] Add system tray icon
- [ ] Persist settings across sessions
- [ ] Battery level notifications
- [ ] Support for other Apple audio devices
