---
name: Frontend log channel bypasses tracing intentionally
description: logging.rs has a FrontendLogger that writes JSONL directly instead of using tracing::*! — don't "fix" it
type: feedback
---
`src-tauri/src/logging.rs` defines a `FrontendLogger` that writes IPC log entries from the webview directly to `widget.jsonl` (sharing the existing `non_blocking::NonBlocking` writer) instead of forwarding them through `tracing::*!` macros. This looks like an inconsistency at first glance, but it's deliberate.

**Why:** Two limitations of `tracing` made the macro path unsuitable for an IPC-mediated log channel.

How to apply: when touching `logging.rs` or the `frontend_log` Tauri command, preserve the direct-write path. Don't refactor it to call `tracing::debug!(target: "frontend", ...)`.

- `tracing` macros require field names known at compile time, so a generic `frontend_log(level, message, data)` IPC command can't translate runtime keys into structured tracing fields.
- The default `tracing-subscriber` JSON formatter renders nested values via `Display`, producing escaped strings inside `fields` instead of nested JSON. That breaks queryability of the structured `data` payload from the frontend.
- `FrontendLogger` emits exactly the same envelope shape (`timestamp`/`level`/`fields`/`target`) as the tracing JSON formatter, so frontend and backend lines interleave cleanly in `widget.jsonl`. Same single source of truth for log files; no separate sink to tail.
