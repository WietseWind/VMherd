#!/usr/bin/env python3
"""Mac App Store screenshots and website screenshots from the built-in demo cluster.

1. Captures every scene with `vmherd --demo --scene <name>` (needs a build with the store-shots
   feature; the app window shows briefly) into dist/store-shots/raw/ (2880x1800, no alpha).
2. Frames them with tools/store-shots/frame.html in headless Chrome: dist/store-shots/appstore/
   NN-name.png, 2880x1800, no alpha channel (App Store Connect rejects alpha).
3. Writes the raw captures at 1600x1000 (png + webp) to website/assets/shots/.

Usage: tools/store-shots/make.py [--no-capture] [--only name,...]
Needs: Google Chrome, ImageMagick (magick), cwebp or magick with webp support, sips.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RAW = ROOT / "dist/store-shots/raw"
OUT = ROOT / "dist/store-shots/appstore"
SITE = ROOT / "website/assets/shots"
FRAME = ROOT / "tools/store-shots/frame.html"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
BG = "#0a0c0b"

# Order = order on the App Store (the first three show in search results).
SCENES = [
    {
        "scene": "broadcast", "n": "01", "eyebrow": "Broadcast",
        "title": "Type once. *Every console follows.*",
        "sub": "Press Enter and you're ON AIR: every keystroke goes to all synced consoles as real key presses.",
        "chips": ["`apt update` × 8", "hold `Esc` 1 s to stop", "`⌘V` types the clipboard"],
        "lens": {"src": [416, 112, 664, 260], "scale": 0.72, "at": [800, 250],
                 "tag": "same keys, every synced console"},
    },
    {
        "scene": "grid", "n": "02", "eyebrow": "Live grid",
        "title": "Every VM console. *One window.*",
        "sub": "Live consoles of your Proxmox VE VMs and containers, side by side. No browser tabs, no noVNC windows.",
        "chips": ["VMs + containers", "auto layout, up to 8 columns", "reconnects by itself"],
        "lens": {"src": [76, 114, 664, 250], "scale": 0.72, "at": [800, 250], "tag": "a live console per VM"},
    },
    {
        "scene": "placeholders", "n": "03", "eyebrow": "Per-VM values",
        "title": "Type it once. *Every VM gets its own.*",
        "sub": "Placeholders are filled in per machine: `hostnamectl set-hostname {{name}}` names each VM after itself.",
        "chips": ["`{{vmid}}` `{{name}}` `{{node}}`", "`{{ipv4}}` `{{ipv6}}` from the guest", "per-key delay, reliable input"],
        "lens": {"src": [416, 112, 664, 220], "scale": 0.75, "at": [780, 250],
                 "tag": "{{name}} {{ipv4}} → web-01 192.0.2.11"},
    },
    {
        "scene": "picker", "n": "04", "eyebrow": "Find the herd",
        "title": "Any VM. *A few keystrokes away.*",
        "sub": "Search by name, VMID, node, status or tag. A space means AND, a comma means OR.",
        "chips": ["space = AND · comma = OR", "paste a column of VMIDs", "`/` `Tab` `Space` `Enter`"],
        "lens": {"src": [24, 116, 1240, 490], "scale": 0.42, "at": [800, 560], "tag": "\"prod\": 5 VMs, picked in order"},
    },
    {
        "scene": "solo", "n": "05", "eyebrow": "Solo",
        "title": "Or take *just one.*",
        "sub": "Click a console to drive that one VM, keyboard and mouse. No Proxmox web interface needed.",
        "chips": ["click to go solo", "keys + mouse to one VM", "hold `Esc` to let go"],
        "lens": {"src": [416, 112, 664, 240], "scale": 0.72, "at": [800, 250], "tag": "SOLO: this VM only"},
    },
    {
        "scene": "power", "n": "06", "eyebrow": "Power",
        "title": "Power buttons, *right on the tile.*",
        "sub": "Start, shut down or stop each VM. Shutdown and stop ask for a second click, so nothing goes down by accident.",
        "chips": ["start · shutdown · stop", "armed second click", "task status as a toast"],
        "lens": {"src": [416, 1100, 664, 200], "scale": 0.72, "at": [800, 250], "tag": "second click: STOP"},
    },
    {
        "scene": "maximized", "n": "07", "eyebrow": "Full window",
        "title": "One click to *full screen.*",
        "sub": "Watch it boot, get through the installer, then drop back into the grid.",
        "chips": ["watch it boot", "fix the installer", "`Esc` back to the grid"],
    },
]


def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def no_alpha(path):
    out = subprocess.run(["sips", "-g", "hasAlpha", str(path)], capture_output=True, text=True).stdout
    return "hasAlpha: no" in out


def capture(names):
    exe = ROOT / "target/release/vmherd"
    if not exe.exists():
        sys.exit("build first: cargo build --release -p vmherd --features store-shots")
    RAW.mkdir(parents=True, exist_ok=True)
    home = tempfile.mkdtemp()  # nothing may be written, but never the real settings
    for name in names:
        print(f"capture {name}")
        run(str(exe), "--demo", "--scene", name, "--out", str(RAW / f"{name}.png"),
            env={**os.environ, "HOME": home}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if os.listdir(home):
        sys.exit(f"the demo wrote into its HOME ({home}); refusing to continue")
    shutil.rmtree(home)


def frame(s):
    OUT.mkdir(parents=True, exist_ok=True)
    spec = {k: v for k, v in s.items() if k != "scene"}
    spec["shot"] = (RAW / f"{s['scene']}.png").as_uri()
    url = FRAME.as_uri() + "#" + urllib.parse.quote(json.dumps(spec))
    out = OUT / f"{s['n']}-{s['scene']}.png"
    out.unlink(missing_ok=True)
    with tempfile.TemporaryDirectory() as profile:
        chrome = subprocess.Popen(
            [CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--allow-file-access-from-files",
             f"--user-data-dir={profile}", "--force-device-scale-factor=2", "--window-size=1440,900",
             "--virtual-time-budget=3000", f"--screenshot={out}", url],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # Chrome sometimes keeps running after it wrote the screenshot: wait for the file, then stop it.
        for _ in range(120):
            if chrome.poll() is not None or (out.exists() and out.stat().st_size > 0):
                break
            time.sleep(0.5)
        time.sleep(1)
        chrome.terminate()
        chrome.wait(10)
    if not out.exists():
        sys.exit(f"Chrome wrote no screenshot for {s['scene']}")
    run("magick", str(out), "-background", BG, "-alpha", "remove", "-alpha", "off", str(out))
    assert no_alpha(out), out
    print(f"framed {out.relative_to(ROOT)}")


def site(name):
    SITE.mkdir(parents=True, exist_ok=True)
    png, webp = SITE / f"{name}.png", SITE / f"{name}.webp"
    run("magick", str(RAW / f"{name}.png"), "-filter", "Lanczos", "-resize", "1600x1000", "-strip", str(png))
    if shutil.which("cwebp"):
        run("cwebp", "-quiet", "-q", "86", str(png), "-o", str(webp))
    else:
        run("magick", str(png), "-quality", "86", str(webp))
    print(f"site   {png.relative_to(ROOT)} (+ .webp)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-capture", action="store_true", help="reuse dist/store-shots/raw")
    ap.add_argument("--only", help="comma-separated scene names")
    a = ap.parse_args()
    scenes = [s for s in SCENES if not a.only or s["scene"] in a.only.split(",")]
    if not a.no_capture:
        capture([s["scene"] for s in scenes])
    for s in scenes:
        frame(s)
        site(s["scene"])


if __name__ == "__main__":
    main()
