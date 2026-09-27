#!/bin/bash
# Mac App Store build: the universal binary with --features mas, the sandboxed dist/mas/VMherd.app
# signed for the App Store, and dist/mas/VMherd-<version>-<build>.pkg for App Store Connect.
# Usage: tools/package-mas.sh [--validate | --upload | --test-build]
#   (no option)   build, sign, package and verify
#   --validate    then let App Store Connect check the package (xcrun altool --validate-app);
#                 nothing is uploaded
#   --upload      validate, then upload the package (App Store Connect / TestFlight); needs a
#                 committed tree, because the build number names the commit
#   --test-build  only dist/mas-test/VMherd.app: the same binary, sandboxed (sandbox + network
#                 client entitlements only), ad-hoc signed, no provisioning profile. For trying the
#                 App Store build's sandbox on a Mac (on another Mac, remove the quarantine first:
#                 xattr -dr com.apple.quarantine VMherd.app); never for distribution.
# Environment:
#   ASC_KEY_ID, ASC_ISSUER  App Store Connect API key id and issuer id (--validate, --upload); altool
#                           reads the key from ~/.appstoreconnect/private_keys/AuthKey_<ASC_KEY_ID>.p8
#   BUILD_NUMBER            CFBundleVersion (default: commit count of HEAD, see tools/macos-common.sh)
#   MAS_PROFILE             Mac App Store provisioning profile (default: VMherd_Mac_App_Store in
#                           ~/Library/MobileDevice/Provisioning Profiles)
#   APP_IDENTITY, PKG_IDENTITY  signing identities (default: the team's Apple Distribution and
#                           3rd Party Mac Developer Installer certificates)
#   ASC_APP_ID              the app's Apple ID in App Store Connect (--upload)
set -euo pipefail
cd "$(dirname "$0")/.."
source tools/macos-common.sh

mode=${1:-}
case $mode in
  "" | --validate | --upload | --test-build) ;;
  *) sed -n '2,21p' "$0" >&2; exit 2 ;;
esac
APP_IDENTITY=${APP_IDENTITY:-"Apple Distribution: The Integrators BV ($TEAM_ID)"}
PKG_IDENTITY=${PKG_IDENTITY:-"3rd Party Mac Developer Installer: The Integrators BV ($TEAM_ID)"}
MAS_PROFILE=${MAS_PROFILE:-"$HOME/Library/MobileDevice/Provisioning Profiles/VMherd_Mac_App_Store.provisionprofile"}
ASC_APP_ID=${ASC_APP_ID:-6816533992}
entitlements=packaging/macos/VMherd-mas.entitlements
bin=target/mas/universal/vmherd
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
pb() { /usr/libexec/PlistBuddy -c "Print :$1" "$2"; }   # ':' paths: keys may contain dots
same_plist() { python3 -c 'import plistlib, sys; a, b = (plistlib.load(open(f, "rb")) for f in sys.argv[1:]); sys.exit(a != b)' "$1" "$2"; }

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "CFBundleShortVersionString needs three integers, got '$version'"
if [ "$mode" = --validate ] || [ "$mode" = --upload ]; then
  : "${ASC_KEY_ID:?set ASC_KEY_ID (App Store Connect API key id)}" "${ASC_ISSUER:?set ASC_ISSUER (API issuer id)}"
fi
if [ "$mode" = --upload ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  die "commit first: the build number names the commit that is uploaded"
fi
# App Store Connect refuses builds made on a beta macOS (ITMS-90111; beta builds end in a letter).
[[ $(sw_vers -buildVersion) =~ [0-9]$ ]] || die "this Mac runs a beta macOS ($(sw_vers -buildVersion))"
build=$(build_number)

# ---------- binary ----------

build_universal "$bin" target/mas --features mas
check_binary "$bin"
# The sandboxed app starts no other programs (guideline 2.5.2): nothing may import the spawn / exec family.
spawn=$(for a in $(lipo -archs "$bin"); do xcrun nm -u -arch "$a" "$bin"; done | sed 's/\$.*//' |
  grep -xE '_(posix_spawnp?|fork|vfork|execl[ep]?|execv[pP]?|execve|execvpe|system|popen)' | sort -u || true)
[ -z "$spawn" ] || die "the App Store binary imports process functions: ${spawn//$'\n'/ }"
echo "no process-spawning imports in $bin"
tools/check-private-apis.sh "$bin"
tools/check-macos-crypto.sh
sdk=$(xcrun --sdk macosx --show-sdk-version)
for a in $(lipo -archs "$bin"); do
  [ "$(vtool -arch "$a" -show-build "$bin" | awk '$1 == "sdk" {print $2}')" = "$sdk" ] || die "$a slice not linked with SDK $sdk"
done

# ---------- bundle ----------

# assemble APP: the bundle, shared by the App Store and the test build.
assemble() {
  local app=$1 res=$1/Contents/Resources plist=$1/Contents/Info.plist
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$res"
  ditto --norsrc --noextattr "$bin" "$app/Contents/MacOS/vmherd"
  ditto --norsrc --noextattr assets/icon/vmherd.icns "$res/vmherd.icns"
  # Apple's standard license agreement covers the App Store version: only the notices of the
  # third-party components ship with it (not LICENSE.md / LICENSE-COMMERCIAL.md).
  ditto --norsrc --noextattr THIRD-PARTY-LICENSES.md "$res/THIRD-PARTY-LICENSES.md"
  sed "s/__VERSION__/$version/g; s/__BUILD__/$build/g" packaging/macos/Info.plist > "$plist"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  # Export compliance: on macOS VMherd uses only the operating system's cryptography
  # (Security.framework: TLS, certificate trust, the VNC DES; tools/check-macos-crypto.sh keeps it
  # that way), which is exempt, so App Store Connect asks no encryption questions per upload.
  plutil -replace ITSAppUsesNonExemptEncryption -bool NO "$plist"
  # The toolchain keys Xcode writes, which App Store Connect reads (missing or wrong: "built with a
  # beta version of Xcode" at submission). Values of the SDK and Xcode that linked the binary.
  local sdkb xv xb major minor patch kv
  sdkb=$(xcrun --sdk macosx --show-sdk-build-version)
  xv=$(xcodebuild -version | awk 'NR == 1 {print $2}')
  xb=$(xcodebuild -version | awk 'NR == 2 {print $3}')
  IFS=. read -r major minor patch <<<"$xv"
  for kv in "DTCompiler com.apple.compilers.llvm.clang.1_0" "DTPlatformName macosx" \
    "DTPlatformVersion $sdk" "DTPlatformBuild $sdkb" "DTSDKName macosx$sdk" "DTSDKBuild $sdkb" \
    "DTXcode $(printf '%02d%d%d' "$major" "${minor:-0}" "${patch:-0}")" "DTXcodeBuild $xb" \
    "BuildMachineOSBuild $(sw_vers -buildVersion)"; do
    plutil -replace "${kv%% *}" -string "${kv#* }" "$plist"
  done
  plutil -lint -s "$plist"
  [ "$(pb LSMinimumSystemVersion "$plist")" = "$MIN_MACOS" ] || die "LSMinimumSystemVersion is not $MIN_MACOS"
  [ "$(pb CFBundleIdentifier "$plist")" = "$BUNDLE_ID" ] || die "CFBundleIdentifier is not $BUNDLE_ID"
}

no_quarantine() {
  local attrs
  attrs=$(xattr -r "$1" 2>/dev/null || true)   # captured: grep -q would cut the pipe (pipefail)
  ! grep -q com.apple.quarantine <<<"$attrs" || die "com.apple.quarantine attributes in $1"
}

if [ "$mode" = --test-build ]; then
  app=dist/mas-test/VMherd.app
  assemble "$app"
  ent=dist/mas-test/VMherd-test.entitlements
  cat > "$ent" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.app-sandbox</key>
	<true/>
	<key>com.apple.security.network.client</key>
	<true/>
</dict>
</plist>
EOF
  xattr -cr "$app"
  codesign --force --sign - --entitlements "$ent" "$app"
  codesign --verify --strict --verbose=2 "$app"
  codesign -d --entitlements - --xml "$app" 2>/dev/null > "$tmp/signed.plist"
  same_plist "$tmp/signed.plist" "$ent" || die "signed entitlements differ from $ent"
  no_quarantine "$app"
  echo "built $app (test only: sandboxed, ad-hoc signed, version $version, build $build)"
  echo "try it: open $app --args --demo   (its container: ~/Library/Containers/$BUNDLE_ID)"
  exit 0
fi

# check_profile: a Mac App Store distribution profile for this app and team, not expired.
check_profile() {
  local p=$tmp/profile.plist
  [ -f "$MAS_PROFILE" ] || die "no provisioning profile at $MAS_PROFILE (set MAS_PROFILE)"
  /usr/bin/openssl smime -inform der -verify -noverify -in "$MAS_PROFILE" -out "$p" 2>/dev/null ||
    die "cannot read the profile $MAS_PROFILE"
  [ "$(pb Entitlements:com.apple.application-identifier "$p")" = "$TEAM_ID.$BUNDLE_ID" ] ||
    die "the profile is for $(pb Entitlements:com.apple.application-identifier "$p"), not $TEAM_ID.$BUNDLE_ID"
  [ "$(pb Entitlements:com.apple.developer.team-identifier "$p")" = "$TEAM_ID" ] || die "the profile is for another team"
  [ "$(pb Platform:0 "$p")" = OSX ] || die "not a macOS profile"
  ! pb ProvisionedDevices "$p" >/dev/null 2>&1 || die "a development profile; use the Mac App Store Connect one"
  ! pb ProvisionsAllDevices "$p" >/dev/null 2>&1 || die "a Developer ID profile; use the Mac App Store Connect one"
  [[ "$(plutil -extract ExpirationDate raw "$p")" > "$(date -u +%FT%TZ)" ]] || die "the profile has expired"
  echo "profile $(pb Name "$p"): $(pb Entitlements:com.apple.application-identifier "$p"), until $(plutil -extract ExpirationDate raw "$p")"
}

app=dist/mas/VMherd.app
pkg=dist/mas/VMherd-$version-$build.pkg
check_profile
assemble "$app"
cp "$MAS_PROFILE" "$app/Contents/embedded.provisionprofile"
xattr -cr "$app"   # App Store Connect refuses com.apple.quarantine anywhere in the app
# Apple's flags for the Mac App Store ("Creating distribution-signed code for macOS"): the one
# executable with the entitlements, no --deep. No hardened runtime and no secure timestamp: those
# are for Developer ID distribution (the App Store re-signs the app).
run_limited 180 codesign --force --timestamp=none --sign "$APP_IDENTITY" --entitlements "$entitlements" "$app"

echo "--- verify $app"
codesign --verify --strict --verbose=2 "$app"
sig=$(codesign -dvv "$app" 2>&1)
grep -E '^(Identifier|Format|Authority|TeamIdentifier|Sealed Resources|Signed Time|Timestamp)' <<<"$sig"
grep -qx "TeamIdentifier=$TEAM_ID" <<<"$sig" || die "not signed by team $TEAM_ID"
grep -q '^Authority=Apple Distribution: ' <<<"$sig" || die "not signed with an Apple Distribution certificate"
codesign -d --entitlements :- "$app" 2>/dev/null > "$tmp/signed.plist"
same_plist "$tmp/signed.plist" "$entitlements" || die "signed entitlements differ from $entitlements"
echo "signed entitlements (same as $entitlements):"
plutil -p "$tmp/signed.plist"
cmp -s "$MAS_PROFILE" "$app/Contents/embedded.provisionprofile" || die "embedded profile differs"
# The signing certificate must be one the profile names (else ITMS-90284).
codesign -d --extract-certificates="$tmp/cert" "$app" 2>/dev/null
python3 -c 'import plistlib, sys; p = plistlib.load(open(sys.argv[1], "rb")); sys.exit(open(sys.argv[2], "rb").read() not in p["DeveloperCertificates"])' \
  "$tmp/profile.plist" "$tmp/cert0" || die "the profile does not list the certificate of $APP_IDENTITY"
echo "embedded profile matches: app id $TEAM_ID.$BUNDLE_ID, signing certificate listed"
no_quarantine "$app"
plutil -p "$app/Contents/Info.plist"

rm -f "$pkg"
run_limited 180 productbuild --component "$app" /Applications --sign "$PKG_IDENTITY" "$pkg"
echo "--- verify $pkg"
pkgsig=$(pkgutil --check-signature "$pkg")
echo "$pkgsig"
grep -q '3rd Party Mac Developer Installer: ' <<<"$pkgsig" || die "package not signed for the App Store"
pkgutil --expand-full "$pkg" "$tmp/pkg"
inner=$(find "$tmp/pkg" -maxdepth 4 -name VMherd.app -type d | head -1)
[ -n "$inner" ] || die "no VMherd.app in $pkg"
codesign --verify --strict "$inner" || die "the app in the package does not verify"
no_quarantine "$tmp/pkg"
no_quarantine "$pkg"
echo "built $pkg (version $version, build $build)"

if [ "$mode" = --validate ] || [ "$mode" = --upload ]; then
  echo "--- App Store Connect: validate"
  xcrun altool --validate-app -f "$pkg" -t macos --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER"
fi
if [ "$mode" = --upload ]; then
  echo "--- App Store Connect: upload"
  xcrun altool --upload-package "$pkg" -t macos --apple-id "$ASC_APP_ID" --bundle-id "$BUNDLE_ID" \
    --bundle-version "$build" --bundle-short-version-string "$version" \
    --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER"
fi
