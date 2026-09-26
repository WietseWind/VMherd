# shellcheck shell=bash disable=SC2034  # (variables are for the scripts that source this)
# Shared by tools/bundle-macos.sh, tools/package-mas.sh and tools/notarize-macos.sh (sourced from
# the repository root; not a script of its own).

# Oldest macOS VMherd runs on: LSMinimumSystemVersion in packaging/macos/Info.plist, and the
# deployment target of both binary slices (rustc's own default for Intel is 10.12).
MIN_MACOS=11.0
TEAM_ID=4Z878WS25G
BUNDLE_ID=app.vmherd

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

die() {
  echo "error: $*" >&2
  exit 1
}

# CFBundleVersion: the number of commits up to HEAD. It only grows as long as releases are built
# from main (never from a rewritten or older branch), which App Store Connect needs: every upload
# must have a higher build number than the ones before, across versions too. BUILD_NUMBER
# overrides it (a plain integer, higher than any build uploaded before).
build_number() {
  local n=${BUILD_NUMBER:-$(git rev-list --count HEAD)}
  [[ $n =~ ^[1-9][0-9]*$ ]] || die "build number must be a positive integer, got '$n'"
  echo "$n"
}

# Compiler settings for release builds: no build-machine paths in the binary, the deployment target
# for both slices, the SDK that xcrun picks (also the one the DT* Info.plist keys name).
macos_build_env() {
  local sep=$'\x1f' flags
  # rustc applies the LAST --remap-path-prefix that matches a path: the generic $HOME rule comes
  # first, the more specific rules (all under $HOME) after it so that they win.
  # CARGO_ENCODED_RUSTFLAGS (0x1f-separated) because the project path may contain spaces.
  flags="--remap-path-prefix=$HOME=/home"
  flags+="${sep}--remap-path-prefix=$HOME/.cargo/registry/src=/cargo"
  flags+="${sep}--remap-path-prefix=$HOME/.cargo/git/checkouts=/cargo-git"
  flags+="${sep}--remap-path-prefix=$HOME/.rustup=/rustup"
  flags+="${sep}--remap-path-prefix=$PWD=/vmherd"
  export CARGO_ENCODED_RUSTFLAGS="$flags"
  unset RUSTFLAGS
  export MACOSX_DEPLOYMENT_TARGET=$MIN_MACOS
  SDKROOT=$(xcrun --sdk macosx --show-sdk-path)
  export SDKROOT
}

# build_universal OUT_BINARY TARGET_DIR [cargo build args...]: release build for Apple silicon and
# Intel, joined with lipo.
build_universal() {
  local out=$1 dir=$2
  shift 2
  macos_build_env
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  local t
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked -p vmherd --target "$t" --target-dir "$dir" "$@"
  done
  mkdir -p "$(dirname "$out")"
  lipo -create -output "$out" "$dir/aarch64-apple-darwin/release/vmherd" "$dir/x86_64-apple-darwin/release/vmherd"
}

# check_binary BINARY: every slice has LC_BUILD_VERSION minos $MIN_MACOS, and no build-machine path
# (home directory, user name) is left in its strings.
check_binary() {
  local bin=$1 arch minos
  for arch in $(lipo -archs "$bin"); do
    minos=$(vtool -arch "$arch" -show-build "$bin" | awk '$1 == "minos" {print $2}')
    [ "$minos" = "$MIN_MACOS" ] || die "$bin ($arch): minimum macOS '${minos:-none}', expected $MIN_MACOS"
  done
  if strings -a "$bin" | grep -qF -e "$HOME" -e "/Users/"; then
    die "$bin still contains $HOME or /Users/ (build-machine paths)"
  fi
}

# run_limited SECONDS COMMAND...: codesign and productbuild wait silently while macOS asks whether
# they may use a private key. Stop instead of hanging, with a hint.
run_limited() {
  local secs=$1 status=0
  shift
  perl -e 'alarm shift; exec @ARGV or die "cannot run $ARGV[0]: $!\n"' "$secs" "$@" || status=$?
  if [ "$status" -eq 142 ]; then
    die "$1 did not finish within $secs s. macOS is probably asking for access to the signing key: \
click \"Always Allow\" in that dialog (enter the login password), then run this script again."
  fi
  return "$status"
}
