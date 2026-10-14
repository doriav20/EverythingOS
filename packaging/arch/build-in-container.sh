#!/usr/bin/env bash
# Builds the Arch packages in an archlinux container (docker compose
# --profile package run --rm package-arch) and smoke-tests them.
set -euo pipefail
pacman -Syu --noconfirm --needed base-devel rust gtk4 libadwaita >/dev/null
useradd -m builder 2>/dev/null || true
# makepkg runs unprivileged and needs a writable copy of the checkout.
rm -rf /tmp/src && mkdir /tmp/src
tar -C /work --exclude=./dist --exclude=./target -cf - . | tar -C /tmp/src -xf -
chown -R builder /tmp/src
su builder -c 'cd /tmp/src/packaging/arch && makepkg -f --noconfirm'
mkdir -p /work/dist
cp /tmp/src/packaging/arch/*.pkg.tar.zst /work/dist/
pacman -U --noconfirm /tmp/src/packaging/arch/*.pkg.tar.zst >/dev/null
evosd --version
evos --version
ls -l /work/dist/*.pkg.tar.zst
