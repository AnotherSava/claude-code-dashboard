---
name: debug_auto_resize_dpi_drift
description: Auto-resize drift on mixed-DPI multi-monitor; diagnose via widget.jsonl dpr-vs-scale mismatch + xy march
metadata:
  type: project
---

The dev machine runs **mixed-DPI multi-monitor**: 2560×1440 @1.0× primary with a ~1.5× laptop panel positioned **below** it (its y-coords exceed 1440). Auto-resize bugs only reproduce when the widget grows into the second monitor — a single-monitor session can't trigger them. **Down** mode grows the window downward *into* the lower 1.5× panel (Up grows away), which is why Down was affected and Up wasn't.

**Diagnostic signature** (grep `widget.jsonl`):
- `auto_resize::apply` (Rust) logs `scale` (= `window.scale_factor()`), `new_height_phys`, `new_x`/`new_y`.
- `auto_resize measure` (frontend) logs `desired` (CSS px), `dpr` (`devicePixelRatio`), `inner_height`, `physical` (sent).
- **The tell:** `dpr` ≠ `scale` near the boundary, and `inner_height` stays short of `desired` → permanent false-overflow. The old failure also showed `new_x`/`new_y` marching monotonically across rapid calls, converging toward a screen corner.

**Root cause + fix** (fixed 2026-06-08): Rust's `window.scale_factor()` and the webview's `devicePixelRatio` disagree near a mixed-DPI boundary, so applying a *logical* height under Rust's scale lands the viewport at the wrong size. Fix: frontend sends **physical** px (`desired * devicePixelRatio`); `auto_resize::apply` sets `PhysicalSize` directly, never using `scale_factor` for sizing. Plus: skip `set_position` when target == current pos, and a `suppressResizeUntil` cooldown ignores our own resize echo. Full framework writeup in the `tauri-mixed-dpi-window-sizing` learning.

Related: [[debug_sync_fake_peer]] for the widget.jsonl-grep debugging style.
