---
name: tray_badge_deferred
description: Tray usage badge — deferred work; macOS light/dark contrast and live DPI re-render not done
metadata:
  type: project
---

The tray usage badge (submenu: None / 5h·7d × lights·number → `config.tray_badge`, rendered in `tray_badge.rs` at the native OS tray size) has two known, deliberate gaps:

1. **macOS light/dark contrast** — the **number** modes draw colored digits + a black outline directly, which won't adapt to a dark menu bar (a non-template colored image gets no auto-tint). The **light** modes recolor the app icon itself, so they read fine on either bar. A number-mode fix needs an appearance-aware treatment (neutral backplate, or swap on appearance change). Sizing on macOS is correct — the `tray-icon` crate pins the image to 18pt tall (see [[tauri-tray-icon-sizing]]).

2. **No live DPI re-render** — the badge pixel size is computed from the main window's `scale_factor()` only when `tray_badge::refresh` runs (usage poll, config change, tray menu select). A monitor move to a different DPI won't re-render until the next such event; there's no `WM_DPICHANGED` hook.

`config.tray_badge` is reset by the deploy step that overwrites `config.json` (see [[project_config_wiped_on_deploy]]), but `config/local.json` now sets it to `five_hour_light`, so the dev machine keeps it across deploys; an installed build persists it too.
