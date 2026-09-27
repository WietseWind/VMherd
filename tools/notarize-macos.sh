#!/bin/bash
# Developer ID build for distribution outside the App Store: tools/bundle-macos.sh (universal), then
# sign dist/VMherd.app with the Developer ID certificate (hardened runtime, secure timestamp), zip
# it, have Apple notarize it, staple the ticket and check it with Gatekeeper.
# Result: dist/VMherd-<version>-macos.zip (the stapled app).
# Usage: tools/notarize-macos.sh [--no-submit]   (--no-submit: stop before sending it to Apple)
# Environment: DEVID_IDENTITY (default: the team's Developer ID Application certificate),
#   NOTARY_PROFILE (default: vmherd; a notarytool keychain profile, created once with
#   `xcrun notarytool store-credentials vmherd --key <AuthKey_ID.p8> --key-id <ID> --issuer <ISSUER>`).
set -euo pipefail
cd "$(dirname "$0")/.."
source tools/macos-common.sh
case ${1:-} in "" | --no-submit) ;; *) sed -n '2,9p' "$0" >&2; exit 2 ;; esac
DEVID_IDENTITY=${DEVID_IDENTITY:-"Developer ID Application: The Integrators BV ($TEAM_ID)"}
NOTARY_PROFILE=${NOTARY_PROFILE:-vmherd}
app=dist/VMherd.app
zip=dist/VMherd-$version-macos.zip

tools/bundle-macos.sh
tools/check-private-apis.sh "$app/Contents/MacOS/vmherd"
xattr -cr "$app"
# Hardened runtime without exceptions: VMherd needs no JIT, unsigned executable memory, library
# loading or DYLD variables (wgpu's Metal backend runs under the plain hardened runtime), and
# outside the sandbox it needs no entitlements at all.
run_limited 180 codesign --force --options runtime --timestamp --sign "$DEVID_IDENTITY" "$app"

echo "--- verify $app"
codesign --verify --strict --verbose=2 "$app"
sig=$(codesign -dvv "$app" 2>&1)
grep -E '^(Identifier|Format|CodeDirectory|Authority|TeamIdentifier|Timestamp|Runtime Version)' <<<"$sig"
grep -q '^CodeDirectory .*flags=0x10000(runtime)' <<<"$sig" || die "hardened runtime not set"
grep -q '^Timestamp=' <<<"$sig" || die "no secure timestamp"
grep -q '^Authority=Developer ID Application: ' <<<"$sig" || die "not signed with a Developer ID"
grep -qx "TeamIdentifier=$TEAM_ID" <<<"$sig" || die "not signed by team $TEAM_ID"
[ -z "$(codesign -d --entitlements - --xml "$app" 2>/dev/null)" ] || die "unexpected entitlements"
echo "no entitlements"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"   # ditto keeps the signature's extended data intact

if [ "${1:-}" = --no-submit ]; then
  echo "signed $app and zipped $zip; not submitted (--no-submit). Gatekeeper before notarization:"
  spctl -a -vv -t exec "$app" || true
  exit 0
fi
echo "--- notarize"
out=$(xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1 | tee /dev/stderr)
grep -q 'status: Accepted' <<<"$out" ||
  die "not notarized; the reasons: xcrun notarytool log <id above> --keychain-profile $NOTARY_PROFILE"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl -a -vv -t exec "$app"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"   # again, now with the stapled ticket
echo "notarized $app, stapled, zipped to $zip"
