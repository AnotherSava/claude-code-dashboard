---
created: 2026-10-15 21:58:54
---

# Restore a session parked on a question as BLOCK, not a read DONE

After a dashboard restart, session_restore brings a ⚫ (clean) tab back as Idle only when Claude Code's registry reports the session idle; otherwise it degrades to a read Done (session_restore::restored_status, and read || from_title == Idle in plan). A session that got a prompt while the dashboard was down and is now parked on an AskUserQuestion has registry status "waiting" (waitingFor: "input needed", seen on 2.1.283), which session_registry::Activity::parse folds into Unknown — so it comes back as done-and-read while it is actually waiting on the user. Fix: add a Waiting variant to Activity (or a separate field) so restore can map it to Blocked. Decide first how /api/agents reports it: the roster's activity field today is idle/busy/unknown and its docs promise it never maps onto a Status, so a new value is a wire change for callers. Rare in practice (needs a prompt sent while the dashboard is down, and the question to appear before the restart).
