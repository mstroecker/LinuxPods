.PHONY: all build run clean fmt test tools install uninstall

# Default target
all: fmt build

# Build the main GUI application
build:
	go build -o linuxpods ./cmd/gui

# Build with race detector (for development)
build-race:
	go build -race -o linuxpods ./cmd/gui

# Run the application
run:
	./linuxpods

# Run with GTK inspector (for UI debugging)
run-debug:
	GTK_DEBUG=interactive ./linuxpods

# Build debugging tools
tools:
	go build -o bin/debug_ble ./cmd/debug_ble
	go build -o bin/debug_aap ./cmd/debug_aap
	go build -o bin/debug_bluez_dbus_battery ./cmd/debug_bluez_dbus_battery
	go build -o bin/debug_bluez_dbus_discover ./cmd/debug_bluez_dbus_discover
	go build -o bin/debug_aap_key_retrieval ./cmd/debug_aap_key_retrieval
	go build -o bin/debug_decrypt ./cmd/debug_decrypt
	go build -o bin/debug_aap_noise_control ./cmd/debug_aap_noise_control

# Format code
fmt:
	go fmt ./...

# Run tests
test:
	go test ./...

# Clean build artifacts
clean:
	rm -f linuxpods
	rm -rf bin/

# Installation paths (user-local, no sudo)
PREFIX      ?= $(HOME)/.local
BINDIR      = $(PREFIX)/bin
AUTOSTART   = $(HOME)/.config/autostart
APPDIR      = $(HOME)/.local/share/applications
DESKTOP     = com.linuxpods.app.desktop

# Install binary, autostart entry, and app launcher
install: build
	install -Dm755 linuxpods $(BINDIR)/linuxpods
	mkdir -p $(AUTOSTART) $(APPDIR)
	@printf '[Desktop Entry]\n\
Type=Application\n\
Version=1.0\n\
Name=LinuxPods\n\
Comment=Manage Apple AirPods on Linux\n\
Exec=$(BINDIR)/linuxpods --minimized\n\
TryExec=$(BINDIR)/linuxpods\n\
Icon=$(CURDIR)/assets/tray_icon3.png\n\
Terminal=false\n\
Categories=Utility;System;Audio;\n\
X-GNOME-Autostart-enabled=true\n' > $(AUTOSTART)/$(DESKTOP)
	sed 's/ --minimized//' $(AUTOSTART)/$(DESKTOP) > $(APPDIR)/$(DESKTOP)
	update-desktop-database $(APPDIR) 2>/dev/null || true
	@if ! pgrep -f "$(BINDIR)/linuxpods" >/dev/null 2>&1; then \
		nohup $(BINDIR)/linuxpods --minimized >/dev/null 2>&1 & \
		echo "LinuxPods started in background (PID $$!)"; \
	else \
		echo "LinuxPods is already running"; \
	fi
	@echo "Installed to $(BINDIR)/linuxpods"
	@echo "Autostart: $(AUTOSTART)/$(DESKTOP)"
	@echo "App launcher: $(APPDIR)/$(DESKTOP)"

# Remove binary, autostart entry, and app launcher
uninstall:
	-pkill -f "$(BINDIR)/linuxpods" 2>/dev/null
	rm -f $(BINDIR)/linuxpods
	rm -f $(AUTOSTART)/$(DESKTOP)
	rm -f $(APPDIR)/$(DESKTOP)
	update-desktop-database $(APPDIR) 2>/dev/null || true
	@echo "LinuxPods uninstalled"

# Download dependencies
deps:
	go mod download
	go mod tidy
