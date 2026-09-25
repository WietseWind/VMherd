#!/bin/bash
# Write THIRD-PARTY-LICENSES.md: every crate compiled into vmherd (all platforms) with its license
# text, plus the fonts bundled in the binary. Needs `cargo install --locked cargo-about --features cli`.
set -euo pipefail
cd "$(dirname "$0")/.."
out=THIRD-PARTY-LICENSES.md
cargo about generate --fail -c about.toml -m crates/vmherd/Cargo.toml tools/licenses.hbs > "$out"
{
  echo
  echo "## Fonts bundled in VMherd"
  echo
  echo "Barlow Semi Condensed (Medium, SemiBold, Bold): SIL Open Font License 1.1"
  echo
  echo '```'; cat assets/fonts/OFL-Barlow.txt; echo '```'
  echo
  echo "JetBrains Mono (Regular, Bold): SIL Open Font License 1.1"
  echo
  echo '```'; cat assets/fonts/OFL-JetBrainsMono.txt; echo '```'
  echo
  echo "egui's default fonts (Ubuntu Light, Hack, Noto Emoji, emoji-icon-font) are covered by the"
  echo "epaint_default_fonts entries above (OFL-1.1, Ubuntu Font Licence 1.0, MIT / Apache-2.0)."
} >> "$out"
echo "wrote $out ($(grep -c '^## ' "$out") license sections, $(wc -c < "$out" | tr -d ' ') bytes)"
