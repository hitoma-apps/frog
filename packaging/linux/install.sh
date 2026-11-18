#!/bin/sh
# Installs FROG's desktop entry and icon for the current user, so Wayland desktops show the
# FROG icon in the taskbar and launcher. Run it from the directory that holds this script.
set -e
here=$(cd "$(dirname "$0")" && pwd)
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"
mkdir -p "$apps" "$icons"
install -m 644 "$here/frog.desktop" "$apps/frog.desktop"
install -m 644 "$here/frog.png" "$icons/frog.png"
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$apps" 2>/dev/null || true
fi
echo "Installed FROG's launcher entry and icon. Restart FROG to see the icon."
