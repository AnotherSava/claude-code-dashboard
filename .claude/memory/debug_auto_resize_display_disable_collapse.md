---
name: debug-auto-resize-display-disable-collapse
description: Widget collapses to header-height sliver when the display it's on is disabled; fix is a frontend self-heal retry
metadata:
  type: project
---

Auto-resize collapse distinct from the DPI-drift teleport ([[debug_auto_resize_dpi_drift]]) and the measure races ([[debug_auto_resize_children_race]], [[debug_auto_resize_config_race]]): when a monitor holding the widget is **disabled**, the window collapses to a header-height sliver (often parked at the primary's `0,0` corner) and stays stuck for many minutes.

**Root cause (confirmed from widget.jsonl):** the frontend's `measureAndSend` never sends a bad height — every `desired` is correct. The signature is `auto_resize measure` lines with `inner_height:18` while `desired` is 105/186 and `physical` is correct, yet the *next* read still shows `inner_height:18`: Windows **swallows the `set_size`** issued mid-transition while disabling the monitor. Those retries are all driven by the `resize`-event burst *during* the transition (2–3 within ~1s), then the burst ends while still collapsed, so nothing re-fires a measure → stuck until an unrelated later trigger (session update / manual move), seen once ~19 min later.

**Fix (App.svelte, frontend-only):** a bounded self-heal. When the window is severely collapsed — `overflowing && window.innerHeight * 2 < desired` (the deep-collapse signature; normal one-frame grows sit at ~0.55–0.68 of desired and are excluded) — keep re-applying on a timer (`HEAL_DELAYS = [300,700,1500,3000,5000]`ms, ~10s budget) past the transition until the window catches up, then self-cancel; re-arm on a fresh collapse after the budget is spent. Rust `set_size` starts sticking once the OS settles, so a later retry lands. `heal` counter is added to the measure trace so future recurrences are greppable.

**Not fixed / out of scope:** the position clamp to `0,0` on a stranded window (that's the intended rescue-onto-screen behavior); a stuck-*too-tall* window (cosmetic, no content loss). Can't repro headlessly — verified by build/deploy; confirm live next time a display is toggled.
