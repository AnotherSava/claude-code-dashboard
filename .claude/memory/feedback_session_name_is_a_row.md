---
name: feedback_session_name_is_a_row
description: "why is <name> session …" names a dashboard row — run /investigate <name> first; "claude" is the dotfiles repo's row, not this session
metadata:
  type: feedback
---

When the user asks why "<name> session" is in some state, `<name>` is a row on the dashboard, usually a project's chat_id or display name. Run `/investigate <name>` on that row before answering. Never assume it means the session you are talking in. In particular, "claude" is the row for the claude dotfiles repo, not this agent.

**Why:** on 2026-10-31 "why claude session is done instead of waiting?" was answered about the tauri-dashboard row. A different bug got investigated and fixed, while the row the user meant (frozen by a `continued-in` move, see [[startup_session_restore]]) went unexamined until they asked a second time.

**How to apply:** take the name literally as a row id. Investigate the current session only when the name actually resolves to this row, or when the user says "this session".
