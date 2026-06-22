---
name: startup_session_restore_infeasible
description: Rejected — restoring live local sessions on startup; can't tell closed from idle-open (Claude doesn't hold transcript open)
metadata:
  type: project
---

Restoring the live session list on dashboard startup (so a cold start / deploy isn't blank) was investigated 2026-06-21 and **rejected**.

**Why:** there's no reliable way to tell a *closed* session from a *still-open-but-idle* one.
- Transcripts (`~/.claude/projects/*/*.jsonl`) carry **no session-end marker** — a session closed 20h+ ago ends with the same record kinds (`turn_duration`, `mode`, `file-history-snapshot`) as a live one, so content can't say "closed".
- mtime only gives "recently active", not "alive" — it includes a just-closed session and misses an idle-open one.
- The one clean cross-platform liveness signal — "is a process holding the transcript open" — **fails**: verified via Win32 RestartManager against two live sessions, neither transcript was held open. Claude opens-appends-closes per write, so between writes (exactly when a session is idle) there's no handle.
- Process→cwd mapping is reliable on macOS (`lsof`) but not Windows → fails the cross-platform bar.

**How to apply:** don't re-attempt startup local-session restore without a *new* liveness signal (e.g. a pid/lock file Claude writes, or accepting the mtime-only approximation despite false positives). Remote sessions need nothing — they self-restore via sync peer pushes + 90s TTL regardless of how long the dashboard was down. Empty-until-reported on cold start is the accepted resting state. The empirical "probe before declaring infeasible" check here matched [[feedback_check_remote_before_fixing]] discipline; see also [[terminal_promote_to_working_unsafe]] (another rejected liveness approach).
