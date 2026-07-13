---
name: auto_resize_dpr_flicker_collapse
description: Widget collapsing to a header sliver = WebView2 devicePixelRatio misreading 1.5→1.0 on mixed-DPI; fixed by sizing against the OS scale_factor from Rust instead of devicePixelRatio (the earlier hysteresis filter was insufficient and removed 2026-07-12)
metadata: 
  node_type: memory
  type: project
---

Symptom: the main widget shrinks to ≈header height (and looks tiny overall) and oscillates, on a mixed-DPI multi-monitor setup (e.g. 4K@150% + a 100% display).

Root cause: `window.devicePixelRatio` transiently misreads (1.5→1.0 then back) in WebView2 near/across a DPI boundary. Auto-resize sizes the window as `physical = desired_css * dpr`; `desired` is stable CSS px, so a low dpr misread lands a collapsed physical height (e.g. `phys=105` shrinks a ~219px window to ~70 logical px). Sends only fire on content change or overflow, so a session update coinciding with a misread triggers the collapse → oscillation. Signature in `widget.jsonl`: `auto_resize measure` lines where `raw_dpr` flips but the physical is wrong.

First fix (hysteresis, INSUFFICIENT — removed): App.svelte `resolveScale` kept the last stable `devicePixelRatio` and adopted a change only after it persisted `SCALE_ADOPT_MS` (800ms). It assumed the misread was a *sub-second flicker*. It isn't — the 1.0 reading holds for **tens of seconds** (88s gaps seen), so it outlives the 800ms window, gets adopted as stable, and the code then confidently collapses the window. Measured ~9% of `auto_resize measure` lines resolved to `dpr:1` post-hysteresis → constant collapse, self-healing only when a later content change re-measured at 1.5.

Real fix (2026-07-12, sizing against the OS scale): stop trusting `window.devicePixelRatio` for the conversion entirely. The Rust side's `window.scale_factor()` reads OS per-monitor DPI and is stable across the WebView2 JS flap. `commands::apply_auto_resize` now RETURNS `scale_factor()` (was `()`), and a new `commands::get_scale_factor` seeds it at mount. App.svelte holds `backendScale` (updated on every apply reply + the mount seed) and `effectiveScale()` returns it (falling back to `devicePixelRatio` only for the one pre-seed measure). `resolveScale`/`SCALE_ADOPT_MS`/`stableScale`/`scheduleScaleRecheck` all deleted. Since `desired` (CSS px, dpr-independent layout) and `backendScale` (OS) are both stable, the misread never enters the math. The measure log still carries `raw_dpr` separately, so a future misread shows as `raw_dpr:1` beside `dpr:1.5` with a correct `physical` — a built-in regression detector (no collapse). Kept from the first fix: `minWidth: 300` in tauri.conf.json and `.list { overflow-x: hidden }`.

NOT a manual edge-drag and NOT a restored narrow `window_position` (that's null; wiped by deploy). Related: [[debug_auto_resize_dpi_drift]] (same DPI family, different signature — xy march to (0,0)), [[debug_auto_resize_config_race]], [[context_percent_tokens_watcher_only]].

Deploy note: relaunching over a still-running instance collides on port 9077 and can leave the new instance in the `get_config` "state not managed" race (degraded, no measures). Kill the old process before relaunching, or verify a clean single instance after deploy.
