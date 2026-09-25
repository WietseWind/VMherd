#!/bin/bash
# Build and test VMherd on Linux in Docker (Debian, current stable Rust):
#  1. the whole test suite,
#  2. without a Secret Service (no session bus): the keyring is reported unavailable, file/command work,
#  3. with GNOME Keyring on a private session bus: a real save / read / delete.
# Usage: tools/test-linux.sh      (needs Docker; caches cargo downloads and builds in Docker volumes)
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm -t \
  -v "$PWD":/src:ro -v vmherd-cargo:/usr/local/cargo/registry -v vmherd-target:/target \
  -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=4 -w /src rust:1-bookworm bash -euo pipefail -c '
    apt-get update -qq >/dev/null
    apt-get install -y -qq --no-install-recommends pkg-config libxkbcommon-dev libwayland-dev libx11-dev \
      libxrandr-dev libxi-dev libgl1-mesa-dev dbus dbus-x11 gnome-keyring >/dev/null
    rustc --version
    echo "== test suite"; cargo test --workspace --quiet 2>&1 | grep -E "test result|FAILED|panicked"
    echo "== without a Secret Service"; cargo test -p vmherd --quiet -- --ignored without_secret_service 2>&1 | grep -E "test result|FAILED|panicked"
    echo "== with GNOME Keyring"
    dbus-run-session -- bash -c "printf test | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1; \
      cargo test -p vmherd --quiet -- --ignored credential_store_roundtrip 2>&1 | grep -E \"test result|FAILED|panicked|not available\""
    echo "== release build"; cargo build --release -p vmherd --quiet && ls -la /target/release/vmherd
  '
