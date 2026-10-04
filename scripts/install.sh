#!/bin/sh
# Installs LinuxPods for the current user: the binary, the icons, the launcher
# entry and the autostart entry. Everything is user-local, so none of it needs
# sudo. `install.sh --uninstall` removes it all again.
#
# BIN and ICONS default to the release tarball's layout next to this script;
# `make install` points them at the build output and the checkout.
set -eu

here=$(dirname "$(readlink -f "$0")")
BIN=${BIN:-$here/linuxpods}
ICONS=${ICONS:-$here/icons}
PREFIX=${PREFIX:-$HOME/.local}

APP_ID=io.github.mstroecker.LinuxPods
BINDIR=$PREFIX/bin
ICONDIR=$PREFIX/share/icons/hicolor
APPDIR=$PREFIX/share/applications
AUTOSTART=$HOME/.config/autostart
# The app ID before the Flathub-style rename. An autostart entry left under it
# would start a second instance: GApplication only deduplicates within one ID.
LEGACY_ID=com.linuxpods.app

remove_legacy() {
    rm -f "$ICONDIR/scalable/apps/$LEGACY_ID.svg" \
        "$ICONDIR/symbolic/apps/$LEGACY_ID-symbolic.svg" \
        "$AUTOSTART/$LEGACY_ID.desktop" \
        "$APPDIR/$LEGACY_ID.desktop"
}

refresh_caches() {
    gtk-update-icon-cache -qtf "$ICONDIR" 2>/dev/null || true
    update-desktop-database "$APPDIR" 2>/dev/null || true
}

if [ "${1:-}" = --uninstall ]; then
    remove_legacy
    pkill -f "^$BINDIR/linuxpods" 2>/dev/null || true
    rm -f "$BINDIR/linuxpods" \
        "$ICONDIR/scalable/apps/$APP_ID.svg" \
        "$ICONDIR/symbolic/apps/$APP_ID-symbolic.svg" \
        "$AUTOSTART/$APP_ID.desktop" \
        "$APPDIR/$APP_ID.desktop"
    refresh_caches
    echo "LinuxPods uninstalled"
    exit 0
fi

remove_legacy
install -Dm755 "$BIN" "$BINDIR/linuxpods"
# The battery artwork is compiled into the binary. These icons go into the theme
# because the shell draws the launcher and the tray, and cannot read resources
# inside the binary. hicolor/index.theme is deliberately not installed: it exists
# for source checkouts, and here the icons merge with the system hicolor index,
# which already declares scalable/apps and symbolic/apps. A second index.theme in
# this base dir would shadow that for every other app's icons.
install -Dm644 "$ICONS/hicolor/scalable/apps/$APP_ID.svg" \
    "$ICONDIR/scalable/apps/$APP_ID.svg"
install -Dm644 "$ICONS/hicolor/symbolic/apps/$APP_ID-symbolic.svg" \
    "$ICONDIR/symbolic/apps/$APP_ID-symbolic.svg"

mkdir -p "$AUTOSTART" "$APPDIR"
cat > "$AUTOSTART/$APP_ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=LinuxPods
Comment=Manage Apple AirPods on Linux
Exec=$BINDIR/linuxpods --minimized
TryExec=$BINDIR/linuxpods
Icon=$APP_ID
Terminal=false
Categories=AudioVideo;Audio;
X-GNOME-Autostart-enabled=true
EOF
sed -e 's/ --minimized//' -e '/^X-GNOME-Autostart-enabled/d' \
    "$AUTOSTART/$APP_ID.desktop" > "$APPDIR/$APP_ID.desktop"
refresh_caches

if pgrep -f "^$BINDIR/linuxpods" >/dev/null 2>&1; then
    echo "LinuxPods is already running - restart it to pick up this build"
else
    nohup "$BINDIR/linuxpods" --minimized >/dev/null 2>&1 &
    echo "LinuxPods started in the tray (PID $!)"
fi
echo "Installed:  $BINDIR/linuxpods"
echo "Icons:      $ICONDIR/{scalable,symbolic}/apps"
echo "Autostart:  $AUTOSTART/$APP_ID.desktop"
echo "Launcher:   $APPDIR/$APP_ID.desktop"
