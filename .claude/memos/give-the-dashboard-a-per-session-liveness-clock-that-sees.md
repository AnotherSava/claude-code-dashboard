---
created: 2026-09-24 14:17:30
---

# Give the dashboard a per-session liveness clock that sees subagent work, from token_scan's existing subagents walk

Nothing in the dashboard can tell that a session running a long subagent is alive. `AgentSession::updated` is the only "sign of life" clock, and it is bumped by the hook stream and by log_watcher tailing the session's own transcript — which never opens `<session>/subagents/**.jsonl`. So from the moment an agent hands work to a subagent until the result comes back, the row looks identical to a wedged one.

Measured during the idle_awake work (2026-09-24, over this machine's transcript history):
- p50 between consecutive intra-turn transcript writes is 7.7s, p90 26.1s, p99 184s — so `updated` is a good liveness clock for ordinary streaming and tool churn.
- Zero `isSidechain` entries appear in main transcripts (140,956 entries checked); all 1,525 subagent transcripts live under `subagents/`, and `infer_state`/`extract_text_entries` skip sidechain entries anyway.
- Separately, 17 intra-turn gaps exceeded 15 minutes over 30 days with an ordinary (non-subagent) tool in flight, so a long single tool call is a second, smaller blind spot on the same clock.

Two shipped features already pay for this:
- `idle_awake`'s silence cap is 30 minutes rather than the ~15 the gap percentiles would otherwise support, purely to outlast a subagent run. A release landing on a live turn causes the exact sleep the feature prevents.
- `waiting_settle` excludes subagent-held WAITs from its time backstop outright (`waiting_backstop_armed` is only set for silently-killable shell tasks), because there is no way to tell a running subagent from a finished one.

The signal already exists on disk and is already being read: `token_scan.rs` walks `~/.claude/projects/**/*.jsonl` including the nested `subagents/` directories every 60s (its own doc comment at the top explains that this is exactly why it is not an extension of log_watcher — 616 of 660 files carrying 55.5% of all output tokens are under `subagents/`). It keeps a per-file byte cursor in `token_scan_cursor.json`.

What is missing is the mapping from a scanned subagent file back to the row that owns it, and a place to put "this row was last seen alive at T" that is NOT `AgentSession::updated` — that field is the compare-and-swap guard `AppState::take_session` (the liveness reaper) and `settle_stale_waiting` both abort on, so writing it from a scanner would silently reset the reaper's dead-streak counter and cancel in-flight WAIT settles.

Worth doing because it would let both features above tighten: idle_awake's cap could come down toward the measured distribution, and waiting_settle could cover subagent WAITs instead of excluding them.
