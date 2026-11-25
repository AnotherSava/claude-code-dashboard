---
created: 2026-11-25 12:01:43
platform: macos
---

# Make the macOS dashboard quit on an AppleScript quit event

On 2026-11-25 a session in the claude repo found Claude Code Dashboard (macOS) ignoring an AppleScript `quit` (osascript -e 'tell application "Claude Code Dashboard" to quit'); it exited only on SIGTERM. The app runs with ActivationPolicy::Accessory and quits through the tray's std::process::exit(0), so the likely gap is that no handler answers the kAEQuitApplication Apple event (RunEvent::ExitRequested may be prevented, or the event never reaches tao). Next step: reproduce with osascript, read widget.jsonl for an exit-requested line, then make an external quit request exit cleanly (the lid_awake deadman already covers an abrupt exit).
