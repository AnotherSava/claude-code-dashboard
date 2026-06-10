---
name: tray_badge_deferred
description: Tray usage badge — deferred work; macOS light/dark contrast and live DPI re-render not done
metadata:
  type: project
---

The tray usage badge (tray "Tray usage badge" submenu none/5h/7d → `config.tray_badge`, rendered in `tray_badge.rs` via `fontdue` at the native OS tray size) has two known, deliberate gaps:

1. **macOS light/dark contrast** — it renders a non-template *colored* icon, so macOS does no light/dark adaptation; the black outline + dimmed-icon background disappear on a dark menu bar. Needs an appearance-aware treatment (neutral/contrasting backplate, or swap on appearance change). Sizing on macOS is correct (the `tray-icon` crate pins the image to 18pt tall — see [[tauri-tray-icon-sizing]]).

2. **No live DPI re-render** — the badge pixel size is computed from the main window's `scale_factor()` only when `tray_badge::refresh` runs (usage poll, config change, tray menu select). A monitor move to a different DPI won't re-render until the next such event; there's no `WM_DPICHANGED` hook.

`config.tray_badge` lives in `config.json`, so it's reset to `none` by the deploy step that overwrites config.json (see [[project_config_wiped_on_deploy]]) — re-enable via the tray after each dev deploy; an installed build persists it.
