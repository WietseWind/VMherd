#!/bin/bash
# Build dist/VMherd.app: universal binary (Apple silicon + Intel), icon, ad-hoc signature.
# Usage: tools/bundle-macos.sh [--native]   (--native: only this Mac's architecture, faster)
# To hand it to others: tools/notarize-macos.sh (Developer ID signature + notarization). The Mac
# App Store build is tools/package-mas.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
source tools/macos-common.sh
app=dist/VMherd.app
if [ "${1:-}" = "--native" ]; then
  macos_build_env
  cargo build --release --locked -p vmherd
  bin=target/release/vmherd
else
  bin=target/universal/vmherd
  build_universal "$bin" target
fi
check_binary "$bin"
build=$(build_number)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/vmherd"
cp assets/icon/vmherd.icns "$app/Contents/Resources/vmherd.icns"
cp LICENSE.md LICENSE-COMMERCIAL.md THIRD-PARTY-LICENSES.md "$app/Contents/Resources/"
sed "s/__VERSION__/$version/g; s/__BUILD__/$build/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"
[ "$(plutil -extract LSMinimumSystemVersion raw "$app/Contents/Info.plist")" = "$MIN_MACOS" ] ||
  die "LSMinimumSystemVersion in packaging/macos/Info.plist is not $MIN_MACOS"
codesign --force --sign - "$app" # ad-hoc (one executable, nothing nested: no --deep)
echo "built $app ($(lipo -archs "$app/Contents/MacOS/vmherd"), version $version, build $build)"
