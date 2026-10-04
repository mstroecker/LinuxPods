.PHONY: all build build-release run run-debug test fmt lint secrets hooks clean install uninstall remove-legacy

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

# Scan the full git history for secrets (rules: .gitleaks.toml)
secrets:
	gitleaks git --redact --no-banner .

# Install the pre-commit secret scan into this checkout
hooks:
	ln -sf ../../scripts/pre-commit "$$(git rev-parse --git-path hooks)/pre-commit"

clean:
	cargo clean

# Installation paths - all user-local, so none of this needs sudo
PREFIX    ?= $(HOME)/.local
BINDIR     = $(PREFIX)/bin
AUTOSTART  = $(HOME)/.config/autostart
APPDIR     = $(PREFIX)/share/applications
ICONDIR    = $(PREFIX)/share/icons/hicolor
DESKTOP    = io.github.mstroecker.LinuxPods.desktop
# The app ID before the Flathub-style rename. An autostart entry left under it
# would start a second instance: GApplication only deduplicates within one ID.
LEGACY_ID  = com.linuxpods.app

# The battery artwork is compiled into the binary, so the installed copy does not
# depend on this checkout. The icons below go into the theme because the shell
# draws the launcher and the tray, and cannot read resources inside the binary.
define DESKTOP_ENTRY
[Desktop Entry]
Type=Application
Version=1.0
Name=LinuxPods
Comment=Manage Apple AirPods on Linux
Exec=$(BINDIR)/linuxpods --minimized
TryExec=$(BINDIR)/linuxpods
Icon=io.github.mstroecker.LinuxPods
Terminal=false
Categories=AudioVideo;Audio;
X-GNOME-Autostart-enabled=true
endef
export DESKTOP_ENTRY

# Install the binary, the autostart entry and the launcher entry
install: build-release remove-legacy
	install -Dm755 target/release/linuxpods $(BINDIR)/linuxpods
	# assets/icons/hicolor/index.theme is deliberately not installed: it exists
	# for source checkouts, and here the icons merge with the system hicolor
	# index, which already declares scalable/apps and symbolic/apps. A second
	# index.theme in this base dir would shadow that for every other app's icons.
	install -Dm644 assets/icons/hicolor/scalable/apps/io.github.mstroecker.LinuxPods.svg \
		$(ICONDIR)/scalable/apps/io.github.mstroecker.LinuxPods.svg
	install -Dm644 assets/icons/hicolor/symbolic/apps/io.github.mstroecker.LinuxPods-symbolic.svg \
		$(ICONDIR)/symbolic/apps/io.github.mstroecker.LinuxPods-symbolic.svg
	-@gtk-update-icon-cache -qtf $(ICONDIR) 2>/dev/null || true
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
	@echo "Icons:      $(ICONDIR)/{scalable,symbolic}/apps"
	@echo "Autostart:  $(AUTOSTART)/$(DESKTOP)"
	@echo "Launcher:   $(APPDIR)/$(DESKTOP)"

# Remove the binary, the autostart entry and the launcher entry
uninstall: remove-legacy
	-@pkill -f '^$(BINDIR)/linuxpods' 2>/dev/null || true
	rm -f $(BINDIR)/linuxpods
	rm -f $(ICONDIR)/scalable/apps/io.github.mstroecker.LinuxPods.svg
	rm -f $(ICONDIR)/symbolic/apps/io.github.mstroecker.LinuxPods-symbolic.svg
	-@gtk-update-icon-cache -qtf $(ICONDIR) 2>/dev/null || true
	rm -f $(AUTOSTART)/$(DESKTOP)
	rm -f $(APPDIR)/$(DESKTOP)
	-@update-desktop-database $(APPDIR) 2>/dev/null || true
	@echo "LinuxPods uninstalled"

# Remove what an install under the old app ID left behind
remove-legacy:
	rm -f $(ICONDIR)/scalable/apps/$(LEGACY_ID).svg
	rm -f $(ICONDIR)/symbolic/apps/$(LEGACY_ID)-symbolic.svg
	rm -f $(AUTOSTART)/$(LEGACY_ID).desktop
	rm -f $(APPDIR)/$(LEGACY_ID).desktop
