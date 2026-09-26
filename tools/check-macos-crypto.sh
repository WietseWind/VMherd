#!/bin/bash
# Fail if the macOS build of vmherd would contain a third-party encryption implementation (TLS
# stack, cipher, AEAD, key exchange / signature crypto). On macOS VMherd uses only the operating
# system's cryptography (Security.framework: TLS, certificate trust, the VNC DES), so App Store
# Connect's export compliance question is answered "only uses encryption within Apple's operating
# system". Hashes (SHA-1 for the websocket handshake, SHA-256 fingerprints) are not encryption.
# Usage: tools/check-macos-crypto.sh [target...]   (default: both macOS targets)
# Runs on any host: cargo tree resolves other targets without building for them (no toolchain needed).
set -euo pipefail
cd "$(dirname "$0")/.."
targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(aarch64-apple-darwin x86_64-apple-darwin)
tls='rustls|rustls-.*|tokio-rustls|hyper-rustls|webpki|webpki-roots|aws-lc-.*|ring|openssl|openssl-.*|boring|boring-sys|tokio-boring|mbedtls|mbedtls-sys|wolfssl|wolfssl-sys|s2n-tls|s2n-tls-sys|schannel'
ciphers='cipher|aead|aes|aes-.*|ccm|eax|ocb3|chacha20|chacha20poly1305|salsa20|xsalsa20poly1305|crypto_secretbox|poly1305|ghash|polyval|des|cbc|ctr|ecb|cfb-mode|cfb8|ofb|blowfish|twofish|camellia|cast5|cast6|idea|rc2|rc4|rc5|serpent|sm4|kuznyechik|magma|threefish|belt-block'
pubkey='rsa|dsa|ecdsa|elliptic-curve|p192|p224|p256|p384|p521|k256|sm2|x25519-dalek|curve25519-dalek|ed25519-dalek|ed448-goldilocks|crypto_box|hpke|snow|sodiumoxide|libsodium-sys|orion|age|sequoia-openpgp|pgp'
banned="^($tls|$ciphers|$pubkey)$"
status=0
for target in "${targets[@]}"; do
  crates=$(cargo tree --locked -e normal --target "$target" -p vmherd --prefix none --format '{p}' | awk '{print $1}' | sort -u)
  found=$(grep -E "$banned" <<<"$crates" || true)
  if [ -n "$found" ]; then
    echo "error: $target: third-party cryptography in the vmherd binary:" >&2
    for crate in $found; do
      cargo tree --locked -e normal --target "$target" -p vmherd -i "$crate" >&2
    done
    status=1
  else
    echo "$target: no third-party cryptography ($(wc -l <<<"$crates" | tr -d ' ') crates checked)"
  fi
done
exit "$status"
