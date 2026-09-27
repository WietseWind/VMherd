#!/usr/bin/env python3
"""Stamp website/*.html links to assets/css/site.css and assets/js/site.js with ?v=<content hash>.

The site is served through a CDN that caches CSS and JS for hours; a new hash per change makes
browsers and the CDN fetch the new file right away. Run after editing site.css or site.js
(--check exits 1 when a page links an outdated hash; CI runs it).
"""

import hashlib
import re
import sys
from pathlib import Path

SITE = Path(__file__).resolve().parents[1] / "website"
ASSETS = ["assets/css/site.css", "assets/js/site.js"]


def main():
    check = "--check" in sys.argv[1:]
    stamp = {a: hashlib.sha256((SITE / a).read_bytes()).hexdigest()[:10] for a in ASSETS}
    stale = []
    for page in sorted(SITE.rglob("*.html")):
        text = page.read_text()
        new = text
        for asset, h in stamp.items():
            # any relative prefix (../, /), with or without an old ?v=
            new = re.sub(rf'((?:\.\./|/)*{re.escape(asset)})(\?v=[0-9a-f]*)?"', rf'\1?v={h}"', new)
        if new != text:
            stale.append(page.relative_to(SITE))
            if not check:
                page.write_text(new)
    if check and stale:
        print("outdated asset hashes (run tools/website-cachebust.py):", *stale, sep="\n  ")
        sys.exit(1)
    print("\n".join(f"{a}?v={h}" for a, h in stamp.items()) + ("" if check else f"\nupdated {len(stale)} page(s)"))


if __name__ == "__main__":
    main()
