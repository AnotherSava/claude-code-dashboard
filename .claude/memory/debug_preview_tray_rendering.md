---
name: debug_preview_tray_rendering
description: Eyeball tray_badge/icon rendering via a throwaway #[ignore] PNG test, then remove the test + png dev-dep
metadata:
  type: project
---

To visually verify `tray_badge.rs` rendering (light badge, number badge, alert border), render it to PNG — the render fns are private and their output is OS-tray bitmaps not otherwise viewable.

**Why:** there's no other way to eyeball badge/border output; unit tests confirm pixel correctness but not how it *looks* at tray sizes.

**How to apply:** temporarily add `png = "0.17"` under `[dev-dependencies]` (already in `Cargo.lock` transitively via tauri, so no network fetch), write an `#[ignore]`d test in `tray_badge.rs`'s test module that decodes `icons/icon.png` into an `Image`, calls the render fns at sizes 16/24/32/36, composites onto a background (e.g. black, or nearest-upscaled for clarity), and writes PNGs to the session scratchpad dir. Run with `cargo test --lib <name> -- --ignored --nocapture`, view via Read. **Remove the test and the `png` dev-dep before committing** — same re-add/render/view/remove cycle each time. Source icon geometry (content box, corner radius) was measured this way with a one-off PIL script and baked into the `ICON_*` constants. Relates to [[tray_badge_deferred]].
