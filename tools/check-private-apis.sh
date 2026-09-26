#!/bin/bash
# Fail if the macOS binary uses private Apple APIs (Mac App Store guideline 2.5.1): private
# frameworks, private CoreGraphics / SkyLight / Process Manager functions, C symbols that no public
# SDK header declares, and undocumented Objective-C selectors (the ones winit 0.30 used, and any
# underscore-prefixed selector). Usage: tools/check-private-apis.sh [binary]
# (default: dist/VMherd.app/Contents/MacOS/vmherd, else target/release/vmherd). Needs Xcode.
set -euo pipefail
export LC_ALL=C   # byte order for sort / comm, and fast
cd "$(dirname "$0")/.."
# The fallback only applies without an argument: a wrong explicit path must not check another binary.
bin=${1:-dist/VMherd.app/Contents/MacOS/vmherd}
[ -n "${1:-}" ] || [ -f "$bin" ] || bin=target/release/vmherd
[ -f "$bin" ] || { echo "no binary at $bin (build it first)" >&2; exit 2; }
sdk=$(xcrun --show-sdk-path)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
found="$tmp/found"
: > "$found"

# 1. Linked libraries: only public system frameworks and libraries.
otool -L "$bin" | awk '/^\t/ {print $1}' | sort -u |
  grep -vE '^(/System/Library/Frameworks/[A-Za-z0-9]+\.framework/|/usr/lib/[^/]+\.dylib$)' |
  sed 's/^/links /' >> "$found" || true

# 2. Imported C symbols: no known private families, and every one declared in a public SDK header.
for arch in $(lipo -archs "$bin"); do xcrun nm -u -arch "$arch" "$bin"; done | sed 's/^_//' | sort -u > "$tmp/imports"
grep -E '^(CGS|SLS|CPS)[A-Z]|^_LS' "$tmp/imports" | sed 's/^/imports _/' >> "$found" || true
find -L "$sdk/usr/include" "$sdk/System/Library/Frameworks" -name '*.h' -print0 2>/dev/null |
  xargs -0 cat 2>/dev/null | tr -cs 'A-Za-z0-9_' '\n' | sort -u > "$tmp/header-idents"
# Emitted by the compiler and linker (stack protector, fortify, ARC runtime entry points), not by
# API calls; every Objective-C / Swift app imports them.
abi='^(_{2,3}chkstk_darwin|__bzero|__(memcpy|memmove|memset|strcpy|strlcpy)_chk|__stack_chk_(fail|guard)|dyld_stub_binder'
abi+='|objc_(alloc|alloc_init|autorelease|autoreleasePoolPop|autoreleasePoolPush|autoreleaseReturnValue|copyWeak'
abi+='|destroyWeak|initWeak|loadWeakRetained|release|retain|retainAutorelease|retainAutoreleaseReturnValue'
abi+='|retainAutoreleasedReturnValue|storeStrong|storeWeak))$'
# Symbol variants such as `stat$INODE64` / `close$NOCANCEL` come from asm labels in those headers.
sed 's/\$[A-Z0-9_]*$//' "$tmp/imports" | sort -u | comm -23 - "$tmp/header-idents" | grep -vE "$abi" |
  sed 's/^\(.*\)$/imports _\1 (in no public SDK header)/' >> "$found" || true

# 3. Objective-C selectors (objc2 registers them from plain strings, so scan all strings).
strings -a "$bin" | sort -u > "$tmp/strings"
known='^(_wantsKeyDownForEvent:|_helpCursor|_zoomInCursor|_zoomOutCursor|_moveCursor|_waitCursor'
known+='|_windowResize[A-Za-z]*Cursor|busyButClickableCursor)$'
grep -E "$known" "$tmp/strings" | sed 's/^/selector /' >> "$found" || true
# Any other underscore-prefixed camelCase selector that is not the name of an imported C symbol.
grep -E '^_[a-z]{3,}[A-Za-z0-9]*[A-Z][a-z]{2,}[A-Za-z0-9]*(:([A-Za-z0-9]+:)*)?$' "$tmp/strings" | grep -vE "$known" |
  while read -r s; do grep -qxF "${s#_}" "$tmp/imports" || echo "selector-like string $s"; done >> "$found" || true

if [ -s "$found" ]; then
  sed 's/^/PRIVATE API: /' "$found" >&2
  exit 1
fi
echo "no private Apple APIs found in $bin ($(lipo -archs "$bin"), $(wc -l < "$tmp/imports" | tr -d ' ') imports)"
