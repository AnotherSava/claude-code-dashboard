---
name: minimized-window-reports-iconic-rect
description: A minimized window reports the OS iconic rect, not its own geometry — never read or write window geometry while minimized
metadata:
  type: project
---

A full-screen game minimizes the always-on-top widget, and a **minimized Windows window does not report its own geometry**. Measured on this machine at 144 DPI (`GetSystemMetricsForDpi`):

- `outer_position()` → `(-32000, -32000)` — the iconic origin, off every monitor by construction
- `outer_size()` → `237x39` == `(SM_CXMINIMIZED, SM_CYMINIMIZED)`
- `GetClientRect` → `215x26` — hence the `actual_client_h: 26` in the incident log
- `is_visible()` → **true** (minimizing does not clear `WS_VISIBLE`)
- `rcNormalPosition` stays correct, so the restore itself is fine

**Why:** `auto_resize::apply` read those values as if real, so the anchor math, the work-area clamp (always resolves to a corner) and the carried-over width were all garbage — and it kept writing them for the whole minimized stretch. `ensure_window_on_screen` saw the iconic rect, judged the window stranded, and moved it. `toggle_window`'s `is_visible()` check read a minimized widget as showing and hid it. See [[debug_auto_resize_display_disable_collapse]] for the sibling display-disable case.

**The frontend is structurally blind here:** minimizing never resizes the WebView2 child, so `window.innerHeight` stays frozen at its pre-minimize value across the whole round trip. Observed: `inner_height: 118` and `heal: 0` on every measure while the real client area was 26px — the collapse self-heal (`vh * 2 < desired`) cannot trip, and after restore the dedup (`desired == lastSentHeight && !overflowing`) swallows the corrective pass. Recovery has to originate in Rust.

**The tray owns Show/Hide, not the command.** `commands::toggle_window` is registered and exported as `toggleWindow()` in `src/lib/api.ts`, but **nothing in `src/` calls it** — every real toggle goes through the tray icon's left-click and the "Show / Hide" menu item. Both used to reach a private duplicate in `tray.rs`, so the first version of this fix hardened the dead copy and left the live path broken. The two are now one `commands::toggle_main`. Before fixing any window-management behavior, confirm which copy the user's click actually reaches.

**How to apply:** gate on **minimized**, never on visibility — a window hidden to the tray keeps a real rect, and skipping there would bring the widget back stale on every tray toggle. `tao::is_minimized()` calls Win32 `IsIconic` directly, so it is authoritative even when a *game* (not the user) minimized us. `WindowEvent::Resized` fires for both the minimize and the restore, so fold it to the restore edge (`was && !now`) before acting — re-fitting on every `Resized` loops. Verify by minimizing the live widget with `ShowWindow(SW_MINIMIZE)` and injecting a synthetic session (see [[debug_synthetic_hook_events]]) to force a measure; a DPI-aware probe is required (see [[debug_dpi_unaware_probe_virtualization]]).
