---
name: debug-state-transitions-via-widget-jsonl
description: Diagnose a wrong row label/state from widget.jsonl — run /investigate <agent> (reconstructs state + decision chain), or grep the decision-tagged lines directly
metadata:
  type: project
---

When a row shows the wrong status or label, the cause is a specific state transition — and every status-affecting decision is logged to `widget.jsonl` with a stable `"decision"` field + a human `reason`, keyed by the resolved chat_id. **Read the decision log, don't theorize.**

Fastest path — the project-local `investigate` skill reconstructs an agent's current state and its decision chain from those lines:

    /investigate <agent>     # or: python3 .claude/skills/investigate/investigate.py <agent>
    /investigate             # no name → lists active sessions to choose from

Decision codes: `classify` (adapter event→status; the question path's reason carries the matched `question_reason` rule + an `evidence_snippet` of the assistant text, so "why Awaiting?" needs no transcript), `resume_working` / `correct_to_awaiting` / `correct_to_done` / `revert_cancelled` (watcher + `idle_probe` corrections; `revert_cancelled` logs the reverted-to status), `apply_set` (the `state.rs` transition: `prior_status`, `task_boundary`, `continuation_suppressed`), `session_clear` / `compact_boundary`.

Manual grep when you want the raw lines:

    grep -a '"decision"' widget.jsonl | grep -a '"<chat-id>"'

**Why:** in the 2026-06-09 "row shows `y` as the task" investigation I gave two wrong root causes from reasoning alone — "the row was Done" and "idle_probe false-demoted" — both refuted by the logs, which showed `prior_status: Idle` reached by a *correct* idle_probe revert after the user Esc-cancelled a typo'd reply. And on 2026-06-17, "why is travel-map WAIT" twice required digging into the transcript + the Rust code — which is exactly why the decision log + `investigate` skill were then built (the `classify` reason now spells out the question rule and quotes the assistant text). Pairs with [[debug-synthetic-hook-events]] for generating the events to inspect.
