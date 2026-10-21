---
created: 2026-10-21 04:04:52
platform: windows
---

# Confirm the first live tmux idle close of a Mac-only remote session from widget.jsonl

The tmux close (terminals/tmux.rs, a child of the Windows Composite) ends a remote session in the WSL tmux holder that no Windows terminal shows: it types /exit into an empty composer and waits for has-session to report the session gone. It has only been unit-tested; no live session has gone through it. Check widget.jsonl for the first decision=idle_close line whose key starts with 'tmux|' (python .claude/skills/investigate/investigate.py <row>). Expected on success: outcome=closed, the tmux session gone (wsl.exe -e tmux ls), the Mac's ⇄ tab closed, and no recreated session after the Mac next wakes. A refusal (close_failed) should log once per row and leave the session alone. Preconditions: a remote session that sat CLEAN for idle_close_ms (1h) with only the Mac attached (or nobody), and the claude repo's clean-exit marker change (lib.sh remote_session_ended_marker, cc-session.sh resume) committed and pulled on the Mac, since without it a Mac that slept through the close recreates the session on reconnect.
