---
name: measure-background-task-duration
description: Measure a background task's true runtime from transcripts via the output-file mtime; the task-notification timestamp is flush-time, not completion
metadata:
  type: project
---

To measure a Claude Code background task's real duration from `~/.claude/projects/**/*.jsonl`, do **not** use the `<task-notification>` entry's timestamp as the end — it's when the notice was *flushed* on the next turn, not when the task finished, so an idle gap before the user's next prompt inflates it wildly (a ~5-min deploy read as 44 min). True completion ≈ the **mtime of the task output file** at `%LOCALAPPDATA%\Temp\claude\<mangled-project>\<session-id>\tasks\<task-id>.output`. Join: start = the originating assistant `tool_use` block's timestamp (match the notification's `<tool-use-id>`); end = output-file mtime (match the notification's `<task-id>`, the filename stem).

Finding (2026-07-11, 677 transcripts): genuinely-finite background shell tasks cap ~9 min (p95 ≈ 7 min); the apparent 30–45-min "deploys" were dev-server *launchers* (persistent — the output file keeps getting touched until the server is killed), not finite work. Subagent WAITs similarly cap ~9 min. This calibrated `config.waiting_settle_ms`'s 10-min default. See [[background-task-kill-is-silent]].
