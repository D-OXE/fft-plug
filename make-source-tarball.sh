#!/bin/sh
# Regenerates fft-plug-<version>.tar.gz from the current working tree for PKGBUILD.
# The tarball is a packaging input only; it is git-ignored and not part of the project.
set -eu
cd "$(dirname "$0")"
version=$(grep -m1 '^pkgver=' PKGBUILD | cut -d= -f2)
top="fft-plug-$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/$top"
rsync -a \
  --exclude '.git/' \
  --exclude '.superpowers/' \
  --exclude 'target/' \
  --exclude 'PKGBUILD' \
  --exclude 'make-source-tarball.sh' \
  --exclude '*.tar.gz' \
  --exclude '*.pkg.tar.zst' \
  ./ "$tmp/$top/"
bsdtar -czf "fft-plug-$version.tar.gz" -C "$tmp" "$top"
echo "wrote fft-plug-$version.tar.gz"
