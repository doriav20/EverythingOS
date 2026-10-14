#!/usr/bin/env bash
# Builds release binaries and packages into dist/.
#
#   scripts/package.sh [all|deb|rpm|tarball]
set -euo pipefail
cd "$(dirname "$0")/.."

DIST=${DIST:-dist}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
TARGET=${CARGO_TARGET_DIR:-target}
mkdir -p "$DIST"

cargo build --release --locked -p evosd -p evos-cli -p evos-gtk

deb() {
    cargo deb --no-build --no-strip -p evosd -o "$DIST"
    cargo deb --no-build --no-strip -p evos-gtk -o "$DIST"
}

rpm() {
    cargo generate-rpm --target-dir "$TARGET" -p crates/evosd -o "$DIST"
    cargo generate-rpm --target-dir "$TARGET" -p crates/evos-gtk -o "$DIST"
}

tarball() {
    local name="evos-$VERSION-$ARCH-linux"
    local stage
    stage=$(mktemp -d)/$name
    mkdir -p "$stage/bin" "$stage/share"
    cp "$TARGET/release/evosd" "$TARGET/release/evos" "$TARGET/release/evos-gtk" "$stage/bin/"
    cp -r packaging/systemd packaging/polkit packaging/config packaging/desktop packaging/icons "$stage/share/"
    cp packaging/tarball/install.sh README.md LICENSE "$stage/"
    tar -C "$(dirname "$stage")" -czf "$DIST/$name.tar.gz" "$name"
    rm -rf "$(dirname "$stage")"
}

case "${1:-all}" in
    all) deb; rpm; tarball ;;
    deb) deb ;;
    rpm) rpm ;;
    tarball) tarball ;;
    *) echo "usage: $0 [all|deb|rpm|tarball]" >&2; exit 2 ;;
esac
ls -l "$DIST"
