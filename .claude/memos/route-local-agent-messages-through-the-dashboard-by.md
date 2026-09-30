---
created: 2026-09-29 21:28:17
---

# Route local agent messages through the dashboard by attesting the loopback caller's process

Proposed 2026-09-29 by the dotfiles session. POST /api/message refuses a live local target on purpose: SendMessage carries a kernel-verified sender and a working reply address, and a dashboard-written frame has neither. Needed so a local project with no live session can be queued too.

Proposed fix: resolve the process owning the loopback TCP connection (lsof on macOS; GetExtendedTcpTable on Windows), map that pid to a session through session_registry, and treat the sender as attested rather than claimed. Until then the client keeps SendMessage for live local sessions and would use the dashboard only to queue.

Check: pid-to-session is ambiguous when several sessions share a cwd (the inbox_for / tab_pid refusal rule), and a hook subprocess or skill script is a child of claude, not claude itself, so an ancestor walk to a claude image is needed.
