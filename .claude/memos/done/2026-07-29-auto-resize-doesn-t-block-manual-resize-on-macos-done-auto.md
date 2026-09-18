---
created: 2026-07-28 13:18:00
---

# auto-resize doesn't block manual resize on macOS [DONE: `auto_resize.rs` grew a platform `resize_lock` (install/engage/release)

Windows keeps the WM_NCHITTEST subclass, macOS pins min == max content height via `set_size_constraints` to the height `apply` is about to set, rebuilt from the window's tauri.conf.json bounds each time so the declared `minWidth` survives the four-bounds write. `engage` runs *before* `set_size`: a stale pin would otherwise clamp the resize, and tao applies constraints synchronously while deferring `set_inner_size` to the main queue. Every bound is spelled out (0 / f32::MAX) rather than left `None`, which tao expands to f64::MIN/f64::MAX and hands NSWindow verbatim. Verified on macOS: engaged → a 420x300 resize request left the window at 420x61 with width still free; released → the same request gave 420x300.]
