#!/usr/bin/env bash
# Builds and checks the Arch package from the .deb's file tree. Runs inside an archlinux
# container (the release workflow starts it in the background while the AppImage bundles):
#
#   docker run --rm -v "$PWD:/work" -e VERSION archlinux:latest /work/.github/release/arch-package.sh
#
# Expects /work/drive-$VERSION-x86_64-root.tar.gz; writes /work/arch-dist/*.pkg.tar.zst.
set -euo pipefail

: "${VERSION:?VERSION is not set}"
pacman -Syu --noconfirm --needed base-devel >/dev/null
useradd -m builder

# makepkg refuses to run as root: build in a copy the builder owns.
build=/home/builder/arch
cp -r /work/packaging/arch "$build"
cp "/work/drive-${VERSION}-x86_64-root.tar.gz" "$build/"
cd "$build"
sum="$(sha256sum "drive-${VERSION}-x86_64-root.tar.gz" | cut -d' ' -f1)"
sed -i -e "s/^pkgver=.*/pkgver=${VERSION}/" -e "s/^sha256sums=.*/sha256sums=('${sum}')/" PKGBUILD
chown -R builder: "$build"
su builder -c 'makepkg --nodeps --noconfirm'

pkg="drive-${VERSION}-1-x86_64.pkg.tar.zst"
pacman -Qip "$pkg"
pacman -Qlp "$pkg"
pacman -U --noconfirm "$pkg"
if ldd /usr/bin/drive /usr/bin/drive-app | grep 'not found'; then exit 1; fi
/usr/bin/drive --version || /usr/bin/drive --help

mkdir -p /work/arch-dist
cp "$pkg" /work/arch-dist/
chmod -R a+rwX /work/arch-dist
