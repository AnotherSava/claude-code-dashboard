---
name: debug-simulate-tray-click
description: Drive the tray icon's left-click from a script by posting WM_USER_TRAYICON, so Show/Hide behavior is testable without a human click
metadata:
  type: project
---

The tray icon's left-click and the "Show / Hide" menu item are the **only** user-reachable Show/Hide path (`crate::commands::toggle_main`), so any change to show/hide/minimize behavior is untestable by inspection alone. Drive it from PowerShell by posting the same message Windows posts:

- Find the app's hidden window of class `tray_icon_app` (same pid as `claude-code-dashboard`).
- `PostMessageW(hwnd, 6002, wparam=1, lparam=0x0201)` then the same with `lparam=0x0202` — `WM_USER_TRAYICON` = 6002 in `tray-icon` 0.24.x, lparam carrying `WM_LBUTTONDOWN` / `WM_LBUTTONUP`. The handler synthesizes the `Click` event `tray.rs` matches on (`MouseButton::Left` + `MouseButtonState::Up`).

Constants live in the crate (`tray-icon-*/src/platform_impl/windows/mod.rs`) and have moved between versions — 0.21 and 0.24 disagree on the ids after 6005 — so re-read them rather than trusting a copy.

**Why:** the first attempt at the minimize fix hardened `commands::toggle_window`, which has no callers, while every real click went through a duplicate in `tray.rs`; nothing caught it because the tray path was never exercised. See [[minimized_window_reports_iconic_rect]].

**How to apply:** pair it with `ShowWindow(SW_MINIMIZE)` from the same script to reproduce the full-screen-game scenario, then assert on `IsIconic` plus the `widget.jsonl` decision lines. Probe DPI-aware and declare the `*W` string APIs `CharSet.Unicode` — see [[debug_dpi_unaware_probe_virtualization]] and [[debug_live_widget_testing]].
