#!/bin/sh
# Builds Křižík and installs it for the current user (default) or into PREFIX.
#   ./install.sh                 → ~/.local
#   sudo PREFIX=/usr/local ./install.sh
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.multiflexi.Krizik

cargo build --release --locked
install -Dm755 target/release/krizik "$PREFIX/bin/krizik"
install -Dm644 data/icons/$APP_ID.svg "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 data/$APP_ID.metainfo.xml "$PREFIX/share/metainfo/$APP_ID.metainfo.xml"
# Use an absolute path so the launcher works even if PREFIX/bin is not in PATH.
mkdir -p "$PREFIX/share/applications"
sed "s|^Exec=krizik|Exec=$PREFIX/bin/krizik|" data/$APP_ID.desktop > "$PREFIX/share/applications/$APP_ID.desktop"
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" || true
command -v update-desktop-database >/dev/null && update-desktop-database -q "$PREFIX/share/applications" || true
if [ "$(id -u)" = 0 ]; then
    install -Dm644 data/udev/70-krizik.rules /etc/udev/rules.d/70-krizik.rules
    udevadm control --reload && udevadm trigger --subsystem-match=usb || true
else
    echo "For firmware updates over USB, also install the udev rule:"
    echo "  sudo install -Dm644 data/udev/70-krizik.rules /etc/udev/rules.d/70-krizik.rules"
    echo "  sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=usb"
fi
echo "Installed Křižík to $PREFIX"
