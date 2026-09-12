.PHONY: all build build-release run run-debug test fmt lint clean install uninstall

# Default target
all: fmt build

# Build the application
build:
	cargo build

build-release:
	cargo build --release

# Run the application
run:
	cargo run

# Run with the GTK inspector for UI debugging
run-debug:
	GTK_DEBUG=interactive RUST_LOG=linuxpods=debug cargo run

# Run with protocol tracing (BLE parse/decrypt, AAP packets)
run-trace:
	RUST_LOG=linuxpods=debug cargo run

test:
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets

clean:
	cargo clean

# Installation paths - all user-local, so none of this needs sudo
PREFIX    ?= $(HOME)/.local
BINDIR     = $(PREFIX)/bin
AUTOSTART  = $(HOME)/.config/autostart
APPDIR     = $(PREFIX)/share/applications
DESKTOP    = com.linuxpods.app.desktop

# The binary looks its assets up under CARGO_MANIFEST_DIR, baked in at compile
# time, so the installed copy still reads them from this checkout. Moving or
# deleting the source tree leaves it without icons.
define DESKTOP_ENTRY
[Desktop Entry]
Type=Application
Version=1.0
Name=LinuxPods
Comment=Manage Apple AirPods on Linux
Exec=$(BINDIR)/linuxpods --minimized
TryExec=$(BINDIR)/linuxpods
Icon=$(CURDIR)/assets/tray_icon3.png
Terminal=false
Categories=AudioVideo;Audio;
X-GNOME-Autostart-enabled=true
endef
export DESKTOP_ENTRY

# Install the binary, the autostart entry and the launcher entry
install: build-release
	install -Dm755 target/release/linuxpods $(BINDIR)/linuxpods
	mkdir -p $(AUTOSTART) $(APPDIR)
	@printf '%s\n' "$$DESKTOP_ENTRY" > $(AUTOSTART)/$(DESKTOP)
	@sed -e 's/ --minimized//' -e '/^X-GNOME-Autostart-enabled/d' \
		$(AUTOSTART)/$(DESKTOP) > $(APPDIR)/$(DESKTOP)
	-@update-desktop-database $(APPDIR) 2>/dev/null || true
	@if pgrep -f '^$(BINDIR)/linuxpods' >/dev/null 2>&1; then \
		echo "LinuxPods is already running - restart it to pick up this build"; \
	else \
		nohup $(BINDIR)/linuxpods --minimized >/dev/null 2>&1 & \
		echo "LinuxPods started in the tray (PID $$!)"; \
	fi
	@echo "Installed:  $(BINDIR)/linuxpods"
	@echo "Autostart:  $(AUTOSTART)/$(DESKTOP)"
	@echo "Launcher:   $(APPDIR)/$(DESKTOP)"

# Remove the binary, the autostart entry and the launcher entry
uninstall:
	-@pkill -f '^$(BINDIR)/linuxpods' 2>/dev/null || true
	rm -f $(BINDIR)/linuxpods
	rm -f $(AUTOSTART)/$(DESKTOP)
	rm -f $(APPDIR)/$(DESKTOP)
	-@update-desktop-database $(APPDIR) 2>/dev/null || true
	@echo "LinuxPods uninstalled"
