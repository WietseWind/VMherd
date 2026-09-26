#!/bin/bash
# The built-in demo cluster must never reach a network: fail if the demo crate's normal
# dependency tree contains an HTTP, TLS, websocket or socket crate.
set -euo pipefail
cd "$(dirname "$0")/.."
bad=$(cargo tree -p demo -e normal --prefix none --locked \
  | grep -E '^(reqwest|hyper|hyper-util|rustls|tokio-rustls|native-tls|tungstenite|tokio-tungstenite|tokio-websockets|socket2) ' || true)
if [ -n "$bad" ]; then
  echo "demo crate depends on networking crates:" >&2
  echo "$bad" >&2
  exit 1
fi
echo "demo crate: no networking dependencies"
