#!/bin/sh
# Builds target/FROG-x86_64.AppImage. Needs appimagetool, from
# https://github.com/AppImage/appimagetool, on PATH or named by APPIMAGETOOL.
#
# For an AppImage that runs on older distributions, build on the oldest glibc you need to
# support (for example, an Ubuntu 22.04 container). A binary built here needs this machine's
# glibc, which is newer.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

tool=${APPIMAGETOOL:-appimagetool}
if ! command -v "$tool" >/dev/null 2>&1; then
    echo "appimagetool not found. Put it on PATH, or set APPIMAGETOOL to its path." >&2
    exit 1
fi

cargo build --release --locked

appdir="$root/target/AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin"
install -m 755 target/release/frog "$appdir/usr/bin/frog"
install -m 755 packaging/appimage/AppRun "$appdir/AppRun"
install -m 644 packaging/linux/frog.desktop "$appdir/frog.desktop"
install -m 644 packaging/linux/frog.png "$appdir/frog.png"
ln -sf frog.png "$appdir/.DirIcon"

ARCH=x86_64 "$tool" "$appdir" "$root/target/FROG-x86_64.AppImage"
echo "Built $root/target/FROG-x86_64.AppImage"
