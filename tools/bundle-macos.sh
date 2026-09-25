#!/bin/bash
# Build dist/VMherd.app: universal binary (Apple silicon + Intel), icon, ad-hoc signature.
# Usage: tools/bundle-macos.sh [--native]   (--native: only this Mac's architecture, faster)
set -euo pipefail
cd "$(dirname "$0")/.."
# Keep build-machine paths (home directory, user name) out of the binary's panic/location strings.
# CARGO_ENCODED_RUSTFLAGS (0x1f-separated) because the project path may contain spaces.
sep=$'\x1f'
flags="--remap-path-prefix=$HOME/.cargo/registry/src=/cargo${sep}--remap-path-prefix=$HOME/.cargo/git/checkouts=/cargo-git"
flags+="${sep}--remap-path-prefix=$HOME/.rustup=/rustup${sep}--remap-path-prefix=$PWD=/vmherd${sep}--remap-path-prefix=$HOME=/home"
export CARGO_ENCODED_RUSTFLAGS="$flags"
unset RUSTFLAGS
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=dist/VMherd.app
if [ "${1:-}" = "--native" ]; then
  cargo build --release -p vmherd
  bin=target/release/vmherd
else
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release -p vmherd --target aarch64-apple-darwin
  cargo build --release -p vmherd --target x86_64-apple-darwin
  mkdir -p target/universal
  lipo -create -output target/universal/vmherd target/aarch64-apple-darwin/release/vmherd target/x86_64-apple-darwin/release/vmherd
  bin=target/universal/vmherd
fi
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/vmherd"
cp assets/icon/vmherd.icns "$app/Contents/Resources/vmherd.icns"
cp LICENSE.md LICENSE-COMMERCIAL.md THIRD-PARTY-LICENSES.md "$app/Contents/Resources/"
sed "s/__VERSION__/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
codesign --force --deep --sign - "$app"   # ad-hoc; use a Developer ID + notarization to distribute
if strings -a "$app/Contents/MacOS/vmherd" | grep -q "$HOME"; then echo "warning: the binary still contains $HOME" >&2; fi
echo "built $app ($(lipo -archs "$app/Contents/MacOS/vmherd"), version $version)"
