---
name: verify_macos_window_geometry_via_ax
description: Test macOS window sizing/constraints with System Events AXSize A/B; a sleeping display fakes a "vanished window"
metadata:
  type: reference
---

To prove a macOS window-geometry change at runtime (resize locks, min/max
constraints, anchors), drive it through Accessibility rather than guessing —
`osascript -e 'tell application "System Events" to tell process
"claude-code-dashboard" to set size of window 1 to {W, H}'`, then read the size
back. Setting `AXSize` goes through `setFrame:`, which AppKit constrains the
same way a user edge-drag is, so an A/B (feature on vs off, same request, same
window position) is decisive. Accessibility is already granted here — no prompt.

Two traps that cost real time:

- **A sleeping or locked display makes the window look destroyed.** System
  Events reports `Can't get window 1 … Invalid index (-1719)` / `count of
  windows = 0`, and `screencapture` writes an all-black PNG. The app is fine and
  keeps logging to `widget.jsonl`. Check for a black screenshot before
  suspecting the code; `caffeinate -u` does not reliably wake it.
- **Screen-edge clamping masquerades as the constraint under test.** AppKit's
  `constrainFrameRect:toScreen:` truncates the request at the work-area edge, so
  a "blocked" result can just be the window hitting the screen. Move the window
  somewhere with room first, or the A/B proves nothing.
- **A successful click can leave state behind.** `click menu item …` returns an
  object reference on success, which reads like noise — easy to misjudge as a
  failure, especially when a sleeping-display AX error lands in the same breath.
  Tray → Help → Connect instructions sets a `setupOverride` that resets only when
  the app **relaunches**, so one exploratory click left the onboarding panel
  covering the dashboard (and the window widened to 783 px) for a full day. Undo
  any UI state a test sets, and confirm the undo.

`auto_resize` is `"none"` in the deployed config, so the resize lock is inert
until you flip it — write `auto_resize` in the app-data `config.json` and the
config watcher hot-reloads it (see [[project_config_wiped_on_deploy]] and
[[dashboard_test_port_override]]). Restore it afterwards.
