---
name: auto_resize_dpr_flicker_collapse
description: Widget collapsing to a header sliver = WebView2 devicePixelRatio flickering 1.5↔1.0 on mixed-DPI; fixed with a scale-hysteresis filter in App.svelte
metadata:
  type: project
---

Symptom: the main widget shrinks to ≈header height (and looks tiny overall) and oscillates, on a mixed-DPI multi-monitor setup (e.g. 4K@150% + a 100% display).

Root cause: `window.devicePixelRatio` transiently misreads (1.5→1.0 then back) in WebView2 near/across a DPI boundary. Auto-resize sizes the window as `physical = desired_css * dpr`; `desired` is stable CSS px, so a low dpr misread lands a collapsed physical height (e.g. `phys=105` shrinks a ~219px window to ~70 logical px). Sends only fire on content change or overflow, so a session update coinciding with a misread triggers the collapse → oscillation. Signature in `widget.jsonl`: `auto_resize measure` lines where `raw_dpr` flips but the physical is wrong.

Fix (App.svelte `resolveScale`): keep the last *stable* scale and adopt a changed `devicePixelRatio` only after it persists `SCALE_ADOPT_MS` (800ms) — a genuine monitor DPI change sticks and is adopted, a flicker reverts first and is ignored. `scale` (filtered) is now the dedup key and the multiplier; the measure log carries both `dpr` (=scale used) and `raw_dpr` (the raw reading) so a filtered flicker is visible as `raw_dpr≠dpr` with a correct `physical`. Also added `minWidth: 300` in tauri.conf.json — auto-resize never restores width (backend-preserved), so a DPI event could ratchet the width and hysteresis wouldn't cover it. Separately, `.list` got `overflow-x: hidden` to kill a narrow-width horizontal-scrollbar→stolen-height→vertical-scrollbar cascade.

NOT a manual edge-drag and NOT a restored narrow `window_position` (that's null; wiped by deploy). Related: [[debug_auto_resize_dpi_drift]] (same DPI family, different signature — xy march to (0,0)), [[debug_auto_resize_config_race]], [[context_percent_tokens_watcher_only]].

Deploy note: relaunching over a still-running instance collides on port 9077 and can leave the new instance in the `get_config` "state not managed" race (degraded, no measures). Kill the old process before relaunching, or verify a clean single instance after deploy.
