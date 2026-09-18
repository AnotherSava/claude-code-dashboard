---
created: 2026-06-27 16:31:01
---

# Use explicit SessionStart.source (startup|resume|clear|compact) and SessionEnd.reason…

Use explicit SessionStart.source (startup|resume|clear|compact) and SessionEnd.reason (clear|logout|...) matchers for boundary detection instead of the transcript_path-rotation heuristic + the dormant >50% token-drop inference (state.rs is_new_session, which never fires on the hook path since the adapter doesn't populate input_tokens). source=clear/compact and reason=clear are authoritative signals now available; could simplify http_server.rs boundary handling and let us drop the dead token-drop branch. From the hooks/state-logic review. [DONE: deleted the dead token-drop branch + last_input_tokens plumbing AND the transcript-rotation heuristic; SessionEnd/PreCompact are the authoritative boundary signals. Did NOT adopt source/reason matching — redundant with those existing paths, not a simplification.]
