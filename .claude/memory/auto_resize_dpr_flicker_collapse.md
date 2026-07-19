---
name: auto_resize_dpr_flicker_collapse
description: "Widget collapsing to a header sliver on mixed-DPI = the scale multiplier disagreeing with the webview's real rasterization ratio; final fix (2026-07-18) sizes against window.devicePixelRatio + re-measures on every dpr change (matchMedia). Sizing against Rust scale_factor was tried 2026-07-12 and REVERSED — it goes stale and fails the opposite way."
metadata: 
  node_type: memory
  type: project
---

Symptom: the main widget shrinks to ≈header height (content clipped to a sliver, looks tiny) on a mixed-DPI multi-monitor setup (e.g. 4K@150% + a 100% display). Auto-resize sizes the window as `physical = desired_css * scale`; `desired` is stable CSS px, so whenever `scale` is LOWER than the ratio the webview actually rasterizes at, the window is sized too short and clips its own content.

**The trap: neither scale source is reliable alone — each fails in a different scenario, and from a single `(dpr, scale_factor)` pair you cannot tell which is wrong.**
- `window.devicePixelRatio` (WebView2 JS): briefly misreads at mount — can read 1.0 for ~2s on a 150% monitor before settling to 1.5. But it then correctly tracks reality, *including* the window moving to a different-DPI monitor.
- Rust `window.scale_factor()`: stable, but read at window creation and **goes stale** — it does NOT track the window later landing on a different-DPI monitor.

First fix (hysteresis, 2026-07-12a, INSUFFICIENT — removed): kept the last stable `devicePixelRatio`, adopting a change only after it persisted 800ms. Assumed the misread was a sub-second flicker; the collapsed *symptom* actually held for tens of seconds (because nothing re-fired a measure), so it outlived the window and got adopted as stable.

Second fix (2026-07-12b, size against Rust scale — **REVERSED 2026-07-18**): stopped trusting `devicePixelRatio`; `apply_auto_resize` returned `scale_factor()`, `get_scale_factor` seeded it at mount, App.svelte held `backendScale`/`effectiveScale`/`adoptScale`. Fixed the setup it was tested on, but on a mixed-DPI **relaunch** it failed the opposite way: after the window landed on the 150% monitor, `devicePixelRatio` correctly settled to 1.5 within ~2s while `scale_factor()` stayed **stuck at 1.0 for 35+min**, sizing the window at 1.0 and clipping the 1.5-rendered content. Signature in `widget.jsonl`: many `auto_resize measure` lines with `dpr:1` (the pinned backendScale) beside `raw_dpr:1.5` and a too-small `physical`.

**Final fix (2026-07-18, size against devicePixelRatio + re-measure on dpr change):** `effectiveScale()` returns `window.devicePixelRatio` — the ratio the webview *actually rasterizes at*, so `cssHeight * devicePixelRatio` is by construction the physical height that fits the content, and it tracks monitor moves. The mount transient self-corrects because a settled dpr re-fires a measure: a `matchMedia('(resolution: Ndppx)')` change listener (re-armed each change, in `onMount`) plus the existing `'resize'` listener + heal retries guarantee any dpr change re-sizes the window. **That re-measure-on-change is the real root-cause cure for the stuck-collapse in both directions** — the earlier fixes both fought the wrong half (which scale to trust) instead of guaranteeing a re-measure. Removed: `backendScale`/`effectiveScale` pinning/`adoptScale`, `commands::get_scale_factor`, and `apply_auto_resize`'s scale return (now `()`). Kept: `minWidth:300` in tauri.conf.json + `.list { overflow-x:hidden }` for the width axis; `auto_resize::apply` still logs Rust `scale` beside the frontend `dpr`/`raw_dpr` so a future mismatch is visible in the trace.

NOT a manual edge-drag and NOT a restored narrow `window_position` (null; wiped by deploy). Related: [[debug_auto_resize_dpi_drift]] (same DPI family, different signature — xy march to (0,0)), [[debug_auto_resize_config_race]], [[debug_auto_resize_display_disable_collapse]] (the heal-retry backstop), [[context_percent_tokens_watcher_only]].

Deploy note: relaunching over a still-running instance collides on port 9077 and can leave the new instance in the `get_config` "state not managed" race (degraded, no measures). Kill the old process before relaunching, or verify a clean single instance after deploy.
