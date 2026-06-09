---
name: debug-state-transitions-via-widget-jsonl
description: Diagnose a wrong row label/state by grepping widget.jsonl apply_set lines (prior_status, task_boundary, continuation_suppressed) — read the decision log, don't theorize
metadata:
  type: project
---

When a row shows the wrong status or label, the cause is almost always a specific state transition — and `state::apply_set` already logs every one to `widget.jsonl` via `tracing::debug!`: `id`, `path` (new/existing), `prior_status`, `new_status`, `task_boundary`, `continuation_suppressed`, `input_label`, `new_original_prompt`. Grep those for the session's chat_id and read the actual sequence before proposing a root cause.

    grep -a '"message":"apply_set"' widget.jsonl | grep -a '"id":"<chat-id>"'

The cancel detectors log too (`idle_probe`: "reverted to pre-prompt state"; `log_watcher`: "turn cancelled (interrupt marker)…"), so the full transition chain is visible.

**Why:** in the 2026-06-09 "row shows `y` as the task" investigation I gave two wrong root causes from reasoning alone — "the row was Done" and "idle_probe false-demoted" — both refuted by the logs, which showed `prior_status: Idle` reached by a *correct* idle_probe revert after the user Esc-cancelled a typo'd reply. The decision log would have shown the real chain on the first look. Pairs with [[debug-synthetic-hook-events]] for generating the events to inspect.
