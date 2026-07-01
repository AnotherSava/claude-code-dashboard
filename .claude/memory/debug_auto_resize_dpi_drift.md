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

**Recurrence 2026-06-30 (physical-px fix is incomplete):** the drift came back in **Up** mode — the note above said Up was safe, no longer true. The monitor layout changed: there's now a ~1.5× monitor to the **right** at x≈3943–4603 (dpr 1.5 dominates the log), not just below. Signature in `widget.jsonl`: `auto_resize measure` `dpr` flip-flops **1↔1.5** on consecutive passes while `auto_resize::apply` `scale` stays **1.5** (dpr≠scale), and the window repeatedly **pins to the primary monitor's top-left corner `new_x:0,new_y:0`** — 211× in one day's log — landing on top of whatever's on the primary (Chrome). Visually the widget collapses to ~header height at the top-left corner. The physical-px sizing fix stopped the wrong-*size* landing but **not** the teleport-to-corner: the `auto_resize::apply` work-area clamp + `current_monitor()` detection mishandled a window sitting on the other-DPI monitor and yanked its position to `(0,0)`.

**Fixed 2026-06-30** — the fix is **Rust-only**: `auto_resize::apply` now clamps against the monitor the window's rect **overlaps most** (`WorkAreaBounds::best_overlap` over `available_monitors()`), falling back to the primary only when the window overlaps no monitor; it no longer trusts `current_monitor()`, so an on-screen window's clamp is a no-op and can't teleport. Verified in `widget.jsonl` post-deploy: 0 `(0,0)` landings (was 211/day), position stable at one `new_x`. Immediate un-stick for a build without the fix: drag the header back / restart (position resets).

**A frontend dpr-stability guard was tried and reverted.** The idea: in `App.svelte measureAndSend`, skip a resize on any pass where `devicePixelRatio` differed from the previous pass (boundary crossing) to damp the height flap. It **regressed startup**: the mount-time measure was silently eaten, so the window opened at its default tall height and stayed height-resizable — the resize lock only arms inside `auto_resize::apply`, which never ran. Tell in `widget.jsonl`: pre-change launches fire the first `auto_resize::apply` ~165ms after the `resize-lock subclass installed` line; with the guard, the first apply was minutes late or never. **Don't re-add a dpr-gate on the measure path without guaranteeing the mount/first measure always commits and the lock arms at startup.** The residual height jitter the guard aimed at is minor and mostly disappears once the teleport (Rust fix) stops jamming the window at the boundary corner.

Related: [[debug_sync_fake_peer]] for the widget.jsonl-grep debugging style.
