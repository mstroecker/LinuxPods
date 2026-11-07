#!/bin/sh
# Packs a built binary into the release tarball:
#   dist/cymba-<version>-<arch>-linux.tar.gz
# with the binary, install.sh, the theme icons, LICENSE and README.md.
# Usage: scripts/package.sh [binary]   (default: target/release/cymba)
#
# File order, owners and timestamps are fixed (the last commit's time), so the
# same binary always gives the same tarball.
set -eu

cd "$(dirname "$0")/.."
bin=${1:-target/release/cymba}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
name=cymba-$version-$(uname -m)-linux
app_id=io.github.mstroecker.Cymba

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
root=$stage/$name

install -Dm755 "$bin" "$root/cymba"
install -Dm755 scripts/install.sh "$root/install.sh"
install -Dm644 "assets/icons/hicolor/scalable/apps/$app_id.svg" \
    "$root/icons/hicolor/scalable/apps/$app_id.svg"
install -Dm644 "assets/icons/hicolor/symbolic/apps/$app_id-symbolic.svg" \
    "$root/icons/hicolor/symbolic/apps/$app_id-symbolic.svg"
install -m644 LICENSE README.md "$root/"

mkdir -p dist
tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@$(git log -1 --format=%ct)" -C "$stage" -cf - "$name" |
    gzip -9n > "dist/$name.tar.gz"
echo "dist/$name.tar.gz"
