---
created: 2026-10-04 15:03:08
---

# Bound widget.jsonl, which appends with nothing rotating it

The dashboard's trace log at `%APPDATA%`/`~/Library/Application Support/com.anothersava.claude-code-dashboard/widget.jsonl` grows without limit: every status decision, title write and label pass appends a line and nothing truncates, rotates or ages it out. Measured 2026-10-04 during the disk-reclaim work: the file had reached 193 MB on this Mac, and the `investigate` skill greps it, so it is read as well as written.

It is not a deletion target — the decision log is what `/investigate` reconstructs a session's state from, and `~/.claude` and the app-support tree are both on the reclaim script's never-list. So the fix is rotation in the writer, not a sweep: `logging.rs` sets up the `tracing` subscriber with `tracing-appender`, which already has a rolling-file appender, and the `FrontendLogger` writes the same envelope directly.

Next step: decide the retention the investigate skill actually needs (it resolves an agent's CURRENT state, so it reads the tail rather than the history), then switch `logging.rs` to `tracing_appender::rolling` with that bound, and make `FrontendLogger` follow the same handle so the two halves cannot rotate differently.
