---
name: background-task-kill-is-silent
description: A user-killed background task (e.g. dev server) fires no hook and writes nothing to the transcript, so WAIT can't self-clear
metadata:
  type: project
---

Claude Code emits **no signal** when the user kills a background task via the UI (down-arrow → x): no lifecycle hook, and nothing in the JSONL transcript. Verified against real transcripts + `widget.jsonl` — the WAIT-state gap is totally silent until the user's next prompt. A task that *completes/fails on its own* injects a `<task-notification>` (status `completed`/`failed`/`killed`/`stopped`) that triggers a follow-up turn → next `Stop` → WAIT clears; but that notification is **buffered** and only written when a turn actually runs, so a kill with no follow-up turn writes nothing at all.

Consequence: `Waiting` self-resolves reliably for finite work (tests/CI/builds and subagents), but a killed **persistent** shell task (dev server, headless Chrome) never clears — the reason for the `waiting_settle` time-based backstop (`config.waiting_settle_ms`, default 10 min). The backstop is **type-gated** (2026-07-18): `classify_stop` reads `Stop.background_tasks[].type` (documented `"shell"` | `"subagent"`) and arms it (`waiting_backstop_armed`) only when a **shell** task is in flight; a WAIT held only by non-shell work (subagent, **Workflow**, any other Claude-managed kind) is never time-settled, because each resolves with a completion turn and settling a live one would falsely mark it Done. (An "arm unless all-subagent" default shipped first and wrongly settled a running Workflow — typed neither `shell` nor `subagent` — hence: arm only for `shell`.) (`Stop.background_tasks` counts both kinds; the transcript's `pendingBackgroundAgentCount` counts subagents only.) See [[measure-background-task-duration]] and [[hooks_research_findings]].
