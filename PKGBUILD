# AI Fix - 2026-11-13 13:51 - 移除源码仓库中的个人维护者标识
# Maintainer: fft-plug contributors
# Local package: builds from a source tarball generated out of the working tree.
#   ./make-source-tarball.sh && makepkg -f
# The tarball is not committed (see .gitignore), so its hash is intentionally SKIP.
pkgname=fft-plug
pkgver=0.1.0
pkgrel=1
pkgdesc="Real-time PipeWire spectrum visualizer for niri and Wayland"
arch=('x86_64')
license=('MIT')
depends=('glibc' 'glib2' 'gtk4' 'gtk4-layer-shell' 'libglvnd' 'libpipewire' 'mesa')
makedepends=('cargo' 'pkgconf' 'rust')
# Cargo handles Rust LTO itself. makepkg's global lto option would append -flto=auto
# to CFLAGS/LDFLAGS; the cc-built libspa static shim then becomes LTO bytecode that the
# final rustc link (which does not pass LDFLAGS) cannot resolve.
options=(!lto)
source=("$pkgname-$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
  cd "$pkgname-$pkgver"
  # AI Fix - 2026-11-13 13:51 - 避免二进制调试字符串泄露源码和 Cargo 主目录
  local cargo_home="${CARGO_HOME:-${HOME}/.cargo}"
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$srcdir=/usr/src/debug/$pkgname --remap-path-prefix=$cargo_home=/usr/src/cargo"
  cargo build --release --locked --all-features
}

check() {
  cd "$pkgname-$pkgver"
  cargo test --release --locked --all-targets --all-features
  cargo test --release --locked --no-default-features
}

package() {
  cd "$pkgname-$pkgver"
  install -Dm755 target/release/fft-plug "$pkgdir/usr/bin/fft-plug"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 niri/fft-plug.kdl "$pkgdir/usr/share/doc/$pkgname/niri-fft-plug.kdl"
}
