---
name: debug-console-titles-tool-consoles
description: Log-based way to confirm a terminal-title write landed without inspecting the screen; general Bash-vs-PowerShell console-isolation trap moved to global learnings
metadata: 
  node_type: memory
  type: project
  modified: 2026-08-07T07:41:15.723Z
---

The general "Bash tool has its own hidden console/tty; verify console-level behavior (titles, `GetConsoleProcessList`, tty resolution) via PowerShell on Windows or via ancestor pids on macOS" trap is not specific to this project — it's now documented in the global `windows-terminal-title.md` and `windows-console-screen-read.md` learnings (moved there 2026-08-07; this file previously duplicated it).

What's specific to this app:

**Log-based cross-check, no screen needed (2026-08-04):** `terminal_title.rs`'s `push_title` only logs `"terminal title written"` (DEBUG, `target: claude_code_dashboard_lib::terminal_title`) *after* the OSC write's `.is_ok()` succeeds — so grepping `widget.jsonl` for that line at the timestamp in question confirms the backend-side write landed, independent of whatever the screen shows. Use this instead of `osascript`/`screencapture` when the physical display can't be inspected (locked or asleep — see [[verify_macos_window_geometry_via_ax]] and the global `macos-ax-window-testing` learning for the black-screencapture false-vanish and the `CGSSessionScreenIsLocked` check). A confirmed-successful write with a visually wrong result points at the terminal/shell side (see [[terminal-title-followups]]), not the backend.
