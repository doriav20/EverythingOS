#!/bin/sh
# Installs EverythingOS from the release tarball on any systemd distribution.
#
#   sudo ./install.sh [--prefix /usr/local] [--no-start]
#   sudo ./install.sh --uninstall [--prefix /usr/local]
set -eu

PREFIX=/usr/local
UNINSTALL=0
START=1
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=$2; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        --no-start) START=0; shift ;;
        -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh must run as root (try sudo)" >&2
    exit 1
fi

HERE=$(cd "$(dirname "$0")" && pwd)
UNIT=/etc/systemd/system/evosd.service
POLICY=/usr/share/polkit-1/actions/org.evos.policy
DESKTOP=$PREFIX/share/applications/org.evos.Search.desktop
ICON=$PREFIX/share/icons/hicolor/scalable/apps/org.evos.Search.svg
systemd=0
[ -d /run/systemd/system ] && systemd=1

if [ "$UNINSTALL" -eq 1 ]; then
    [ "$systemd" -eq 1 ] && systemctl disable --now evosd.service 2>/dev/null || true
    rm -f "$PREFIX/bin/evosd" "$PREFIX/bin/evos" "$PREFIX/bin/evos-gtk" "$UNIT" "$POLICY" "$DESKTOP" "$ICON"
    [ "$systemd" -eq 1 ] && systemctl daemon-reload || true
    echo "EverythingOS removed. Configuration in /etc/evos and indexes in /var/lib/evos were kept."
    exit 0
fi

install -Dm755 "$HERE/bin/evosd" "$PREFIX/bin/evosd"
install -Dm755 "$HERE/bin/evos" "$PREFIX/bin/evos"
install -Dm755 "$HERE/bin/evos-gtk" "$PREFIX/bin/evos-gtk"
sed "s#^ExecStart=/usr/bin/evosd#ExecStart=$PREFIX/bin/evosd#" \
    "$HERE/share/systemd/evosd.service" > "$UNIT"
chmod 644 "$UNIT"
install -Dm644 "$HERE/share/polkit/org.evos.policy" "$POLICY"
install -Dm644 "$HERE/share/desktop/org.evos.Search.desktop" "$DESKTOP"
install -Dm644 "$HERE/share/icons/org.evos.Search.svg" "$ICON"
if [ ! -e /etc/evos/evosd.toml ]; then
    install -Dm644 "$HERE/share/config/evosd.toml" /etc/evos/evosd.toml
fi

if [ "$systemd" -eq 1 ]; then
    systemctl daemon-reload
    if [ "$START" -eq 1 ]; then
        systemctl enable --now evosd.service
    else
        systemctl enable evosd.service
    fi
fi
echo "EverythingOS installed to $PREFIX. Search with 'evos <query>' or open EverythingOS from your applications."
