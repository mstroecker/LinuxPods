#!/bin/sh
# Installs Cymba for the current user: the binary, the icons, the launcher
# entry and the autostart entry. Everything is user-local, so none of it needs
# sudo. `install.sh --uninstall` removes it all again.
#
# BIN and ICONS default to the release tarball's layout next to this script;
# `make install` points them at the build output and the checkout.
set -eu

here=$(dirname "$(readlink -f "$0")")
BIN=${BIN:-$here/cymba}
ICONS=${ICONS:-$here/icons}
PREFIX=${PREFIX:-$HOME/.local}

APP_ID=io.github.mstroecker.Cymba
BINDIR=$PREFIX/bin
ICONDIR=$PREFIX/share/icons/hicolor
APPDIR=$PREFIX/share/applications
AUTOSTART=$HOME/.config/autostart
# The app's earlier IDs: com.linuxpods.app before the Flathub-style ID, and
# LinuxPods' up to 0.5.0. An autostart entry left under one would start a
# second instance: GApplication only deduplicates within one ID.
LEGACY_IDS="com.linuxpods.app io.github.mstroecker.LinuxPods"
# LinuxPods' binary, up to 0.5.0.
LEGACY_BIN=$BINDIR/linuxpods

# Stops the instance running from the binary $1 and waits until it has exited,
# so it is not still saving into its directories when Cymba moves them. SIGTERM
# first, which quits it as its menu does; SIGKILL if it is still there after
# 5 seconds.
stop() {
    pkill -f "^$1" 2>/dev/null || return 0
    i=0
    while pgrep -f "^$1" >/dev/null 2>&1; do
        if [ "$i" -eq 70 ]; then
            echo "$1 is still running; quit it before starting Cymba" >&2
            exit 1
        fi
        if [ "$i" -eq 50 ]; then
            echo "$1 did not quit within 5 seconds: killing it"
            pkill -KILL -f "^$1" 2>/dev/null || true
        fi
        sleep 0.1
        i=$((i + 1))
    done
}

remove_legacy() {
    stop "$LEGACY_BIN"
    rm -f "$LEGACY_BIN"
    for id in $LEGACY_IDS; do
        rm -f "$ICONDIR/scalable/apps/$id.svg" \
            "$ICONDIR/symbolic/apps/$id-symbolic.svg" \
            "$AUTOSTART/$id.desktop" \
            "$APPDIR/$id.desktop"
    done
}

refresh_caches() {
    gtk-update-icon-cache -qtf "$ICONDIR" 2>/dev/null || true
    update-desktop-database "$APPDIR" 2>/dev/null || true
}

if [ "${1:-}" = --uninstall ]; then
    remove_legacy
    stop "$BINDIR/cymba"
    rm -f "$BINDIR/cymba" \
        "$ICONDIR/scalable/apps/$APP_ID.svg" \
        "$ICONDIR/symbolic/apps/$APP_ID-symbolic.svg" \
        "$AUTOSTART/$APP_ID.desktop" \
        "$APPDIR/$APP_ID.desktop"
    refresh_caches
    echo "Cymba uninstalled"
    exit 0
fi

remove_legacy
install -Dm755 "$BIN" "$BINDIR/cymba"
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
Name=Cymba
GenericName=AirPods Manager
Comment=Manage Apple AirPods on Linux
Keywords=AirPods;earbuds;battery;headphones;
Exec=$BINDIR/cymba --minimized
TryExec=$BINDIR/cymba
Icon=$APP_ID
Terminal=false
Categories=AudioVideo;Audio;
X-GNOME-Autostart-enabled=true
EOF
sed -e 's/ --minimized//' -e '/^X-GNOME-Autostart-enabled/d' \
    "$AUTOSTART/$APP_ID.desktop" > "$APPDIR/$APP_ID.desktop"
refresh_caches

if pgrep -f "^$BINDIR/cymba" >/dev/null 2>&1; then
    echo "Cymba is already running - restart it to pick up this build"
else
    nohup "$BINDIR/cymba" --minimized >/dev/null 2>&1 &
    echo "Cymba started in the tray (PID $!)"
fi
echo "Installed:  $BINDIR/cymba"
echo "Icons:      $ICONDIR/{scalable,symbolic}/apps"
echo "Autostart:  $AUTOSTART/$APP_ID.desktop"
echo "Launcher:   $APPDIR/$APP_ID.desktop"
