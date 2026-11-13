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
# AI Fix - 2026-11-13 13:51 - 仅打包 Git 跟踪或未忽略的源码，防止带入本地隐私文件
git ls-files --cached --others --exclude-standard -z -- \
  . \
  ':(exclude)PKGBUILD' \
  ':(exclude)make-source-tarball.sh' \
  ':(exclude)pkg/**' \
  ':(exclude)src/fft-plug-*/**' | \
  rsync -a --from0 --files-from=- --ignore-missing-args ./ "$tmp/$top/"
bsdtar -czf "fft-plug-$version.tar.gz" -C "$tmp" "$top"
echo "wrote fft-plug-$version.tar.gz"
