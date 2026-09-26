# Vendored winit 0.30.13 (VMherd patch)

This directory is the crates.io package of [winit](https://github.com/rust-windowing/winit)
0.30.13 (upstream commit `e9809ef54b18499bb4f2cac945719ecc2a61061b`, license Apache-2.0, see
`LICENSE`), with a small patch that removes its use of private macOS APIs. The root `Cargo.toml`
points every use of winit at it:

```toml
[patch.crates-io]
winit = { path = "vendor/winit-0.30.13" }
```

## Why

eframe 0.36 uses winit 0.30, and winit 0.30 calls private Apple APIs on macOS. The Mac App Store
rejects binaries that use non-public APIs (App Store Review Guideline 2.5.1), and VMherd is sold
there. VMherd uses none of the affected features itself.

## The patch

The first commit that adds this directory is the unmodified crates.io copy, so
`git log -p -- vendor/winit-0.30.13` shows the patch exactly. Every change carries a
`VMherd patch` comment. All of it is in `src/platform_impl/macos/` (plus one doc line):

| Private API | Where | Replacement |
|---|---|---|
| `CGSMainConnectionID`, `CGSSetWindowBackgroundBlurRadius` (private CoreGraphics / SkyLight functions) | `ffi.rs` imports, `window_delegate.rs` `set_blur` | Imports removed; `set_blur` is a no-op (window background blur is unsupported on macOS, as on most platforms; `src/window.rs` docs say so). |
| Undocumented `NSCursor` class methods `_helpCursor`, `_zoomInCursor`, `_zoomOutCursor`, `_windowResize{NorthEast,NorthWest,SouthEast,SouthWest,NorthEastSouthWest,NorthWestSouthEast}Cursor`, `busyButClickableCursor` | `cursor.rs` | `ZoomIn` / `ZoomOut`: public `+[NSCursor zoomInCursor]` / `zoomOutCursor` (macOS 15+). Diagonal resize: public `+[NSCursor frameResizeCursorFromPosition:inDirections:]` (macOS 15+). On older macOS, and for `Help`, the system cursor images in HIServices' `Resources/cursors` that winit already loads for `Move` / `Cell` (with a file-exists check, falling back to the arrow). `Wait` / `Progress`: the arrow (there is no public busy cursor). |
| NSView override `_wantsKeyDownForEvent:` (private AppKit hook that lets Ctrl-Tab etc. reach `keyDown:`) | `view.rs` | Public `performKeyEquivalent:` override: a key-down with Control (and without Command), while this view is the key window's first responder, is handled by `keyDown:` and claimed; everything else goes to `super`. |

The keyboard change matters for VMherd, which forwards keys (including Ctrl combinations) to VM
consoles. It was checked by posting synthesized key events through the real AppKit dispatch
(`-[NSApplication postEvent:atStart:]`) to a winit window: with the unpatched winit, with this
patch, and with neither hook. Ctrl-Tab, Ctrl-Shift-Tab, Ctrl-Esc, Ctrl-C, Ctrl-A, Tab and `a` all
arrive as `KeyboardInput` with the unpatched winit and with this patch (with IME allowed or not);
with neither hook, AppKit swallows Ctrl-Tab and Ctrl-Shift-Tab (keyboard focus navigation).

`tools/check-private-apis.sh` (run in CI on the macOS build) fails if a private symbol, private
framework or one of these selectors comes back.

## Dropping it

Upstream winit's next major version (0.31, macOS backend in the `winit-appkit` crate) has a
`private-apple-apis` cargo feature for its private API use. When eframe moves to it, and that
feature is off in VMherd's dependency tree (the check script below confirms what is left):

1. Remove the `[patch.crates-io]` entry from the root `Cargo.toml` and delete `vendor/winit-0.30.13`.
2. `cargo update -p winit` (or the eframe upgrade) so `Cargo.lock` has the crates.io source again.
3. `cargo build --release -p vmherd && tools/check-private-apis.sh target/release/vmherd`, and check
   that Ctrl-Tab still reaches the consoles.

To move to a newer winit 0.30.x instead: copy it from `~/.cargo/registry/src/*/winit-<version>/`
into `vendor/winit-<version>/`, commit it unmodified, re-apply the changes above, update the path
in `Cargo.toml` and run the same checks.
