---
name: verify_macos_window_geometry_via_ax
description: "Project-specific gotchas when AX-testing this dashboard's window; general AX testing technique moved to the global macos-ax-window-testing learning"
metadata: 
  node_type: memory
  type: reference
  modified: 2026-08-07T07:40:58.100Z
---

The general System Events `AXSize` A/B technique, plus the sleeping/locked-display, screen-edge-clamping, and stuck-UI-state traps, now live in the global `macos-ax-window-testing.md` learning (not project-specific — moved there 2026-08-07). This file keeps only what's specific to this app:

- **`setupOverride` sticks across a click.** Tray → Help → Connect instructions sets a `setupOverride` in `App.svelte` that resets only when the app **relaunches** — not on hide/show. One exploratory click during a verification pass left the onboarding panel covering the dashboard (and the window widened to 783 px) for a full day. Undo it and confirm via a relaunch or the explicit dismiss path, not just hiding the window.
- **`auto_resize` is `"up"` in the deployed config as of 2026-08-07** (set via `config/local.json`, see [[project_config_wiped_on_deploy]]) — the resize lock is live by default now, not inert. To test a *different* mode, write `auto_resize` in the app-data `config.json` directly; the config watcher hot-reloads it (see also [[dashboard_test_port_override]]). Restore it afterwards.
