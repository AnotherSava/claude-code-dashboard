---
created: 2026-10-31 04:17:24
---

# Hand a row to the session a continued-in record names, if Claude Code moves a conversation again

Seen once, 2026-10-31 02:44 PDT (09:44 UTC) in the claude dotfiles project, on Claude Code 2.1.296. Build the fix only if it happens again.

What happened: Claude Code, unprompted, moved the conversation of an interactive `claude --continue` session (e39d4f9e) into a new session (cf6166d8) hosted by a `claude daemon run --origin transient --spawned-by {...}` process that the interactive process spawned. The old transcript's last line is
  {"type":"continued-in","sessionId":"e39d4f9e-...","continuedInSessionId":"cf6166d8-..."}
and the new transcript starts with a full copy of the old conversation. The user kept typing in the same agterm tab, and every turn landed in cf6166d8. The interactive process stayed alive (registry kind interactive, status idle since the move), so it never left the row. It was the only continued-in record across every transcript on the Mac at the time.

Effect on the dashboard: the interactive process founded the row and stayed main; the daemon session was admitted as a non-main member, so every one of its events (BLOCK, WAIT, and the /clear SessionStart that would have made the row IDLE) was logged decision=member_event and not applied. The row sat frozen on DONE, and shared_row_alert fired. The hook's agent_pid for the moved session resolves to the daemon pid.

Proposed fix: when an event arrives from a non-main member, read the tail of the main's transcript; if its last record is continued-in naming that member's session id, make the member main (main_change via=continued). That goes through the usual hand-over, which clears the old session's state behind a separator. Decide how shared_row_alert treats a member whose session continued elsewhere, since that front-end process never exits. Add tests for the transcript check and the main change. Diagnose a recurrence with /investigate on the row: member_event lines from a pid running 'claude daemon run' while the main sits idle.
