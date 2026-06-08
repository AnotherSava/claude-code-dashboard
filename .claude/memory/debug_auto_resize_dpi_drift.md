---
name: debug_auto_resize_dpi_drift
description: Auto-resize window drift on mixed-DPI multi-monitor; diagnose via widget.jsonl scale-flip + xy march
metadata:
  type: project
---

The dev machine runs **mixed-DPI multi-monitor** (2560×1440 @1.0× primary + a ~1.5× laptop panel). Window-positioning bugs in `auto_resize.rs` only reproduce when the widget sits on or crosses onto the second monitor — a single-monitor session can't trigger them.

**Diagnostic signature of the resize feedback loop** (grep `widget.jsonl` for `auto_resize::apply`):
- `new_height_phys` differs for the *same* `desired_logical_height` (e.g. 145 → 218) → the window read a different `scale_factor`, i.e. it crossed a DPI boundary mid-loop.
- `new_x`/`new_y` march monotonically across many calls within seconds, converging geometrically toward a screen corner → runaway resize↔reposition loop.

**Root cause** (fixed 2026-06-08): across a DPI boundary the frontend overflow re-trigger (`desired > innerHeight+1`) never clears because `innerHeight` stops matching the `desired` measured on the other monitor, and `apply()` called `set_position` every time — on a frameless window the `outer_position` round-trip is inconsistent across DPI, nudging the window each call. Fix: suppress the self-resize echo (`App.svelte` `suppressResizeUntil`) + skip `set_position` when target == current pos (`auto_resize.rs`).

Related: [[debug_sync_fake_peer]] for the widget.jsonl-grep debugging style.
