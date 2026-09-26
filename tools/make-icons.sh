#!/bin/bash
# Regenerate all icon files from assets/icon/vmherd-prompt.svg (+ vmherd-prompt-small.svg for 16-64 px):
# the neutral `>_` design used for every release. VARIANT=proxmox renders vmherd.svg / vmherd-small.svg
# instead (lead screen with a Proxmox-style X; the Proxmox logo is a trademark of Proxmox Server
# Solutions GmbH, so that variant is for private builds only).
# Needs rsvg-convert (librsvg) and ImageMagick; iconutil (macOS) for the .icns.
set -euo pipefail
cd "$(dirname "$0")/../assets/icon"
if [ "${VARIANT:-}" = proxmox ]; then big=vmherd.svg small=vmherd-small.svg; else big=vmherd-prompt.svg small=vmherd-prompt-small.svg; fi
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
png() { rsvg-convert -w "$2" -h "$2" "$1" -o "$3"; }

png $big 1024 vmherd-1024.png
png $big 512 vmherd-512.png
png $big 256 vmherd-256.png

# macOS .icns
set_=$tmp/vmherd.iconset; mkdir "$set_"
for s in 16 32; do png $small $s "$set_/icon_${s}x${s}.png"; png $small $((s * 2)) "$set_/icon_${s}x${s}@2x.png"; done
for s in 128 256 512; do png $big $s "$set_/icon_${s}x${s}.png"; png $big $((s * 2)) "$set_/icon_${s}x${s}@2x.png"; done
if command -v iconutil >/dev/null; then iconutil -c icns "$set_" -o vmherd.icns; fi

# Windows .ico
for s in 16 24 32 48 64; do png $small $s "$tmp/ico-$s.png"; done
for s in 128 256; do png $big $s "$tmp/ico-$s.png"; done
magick "$tmp"/ico-{16,24,32,48,64,128,256}.png vmherd.ico
ls -la vmherd.icns vmherd.ico vmherd-*.png
