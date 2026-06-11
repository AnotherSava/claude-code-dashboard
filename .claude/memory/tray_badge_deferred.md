---
name: tray_badge_deferred
description: Tray usage badge — number mode is intentionally transparent (a contrast plate was tried + rejected); live DPI re-render still deferred
metadata:
  type: project
---

The tray usage badge (submenu: None / 5h·7d × lights·number → `config.tray_badge`, rendered in `tray_badge.rs` at the native OS tray size) — status of its rough edges:

1. **Number-mode contrast is a deliberate non-fix.** The **number** modes draw colored digits (green→amber→red) on a *fully transparent* background — only the digits carry alpha, nothing behind them. On a blue/bright menu bar the green washes out (the original complaint). A solid opaque black plate behind the digits was implemented and verified to fix this, then **deliberately reverted** (2026-06-11): the user preferred the cleaner floating-digit look and accepted the lower contrast. So do **not** re-add a backplate/outline to "fix" contrast without asking — it was a considered choice. The **light** modes recolor the app icon itself, so they read fine on either bar. Sizing on macOS is correct — the `tray-icon` crate pins the image to 18pt tall (see [[tauri-tray-icon-sizing]]).

2. **No live DPI re-render** — the badge pixel size is computed from the main window's `scale_factor()` only when `tray_badge::refresh` runs (usage poll, config change, tray menu select). A monitor move to a different DPI won't re-render until the next such event; there's no `WM_DPICHANGED` hook.

`config.tray_badge` is reset by the deploy step that overwrites `config.json` (see [[project_config_wiped_on_deploy]]) — re-select the mode from the tray submenu after a deploy (or set it in `config/local.json`).
