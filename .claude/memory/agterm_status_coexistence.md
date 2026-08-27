---
name: agterm-status-coexistence
description: This dashboard is the status source of truth inside agterm; agterm's own glyph is deliberately dark
metadata:
  type: project
---

On macOS the user's terminal is **agterm**, which has its own agent-status glyph fed by Claude Code hooks it installs into `~/.claude/settings.json`. As of 2026-08-27 those hooks are **removed**: this dashboard owns status display, and agterm's glyph stays idle.

**Why:** agterm's four values (idle/active/completed/blocked) cannot express `Waiting` or `Error`, so the two readouts could never agree — and two status signals on one row is a duplicate state signal. What the user gives up (attention navigation, the blocked sound, auto-follow) was already unused.

**How to apply:**
- `terminal_title.rs`'s OSC 0 string *becomes* agterm's session name — that is the intended display here, not a side effect. Setting `terminal_titles: false` removes the dashboard from agterm entirely.
- Do not add a second status path (e.g. calling `agtermctl session status` from `notifications.rs`) — agterm's docs treat the two sources as alternatives, not layers.
- The agterm-side mechanics — OSC-title precedence, the un-re-runnable hook installer, the ghostty `title` shell-integration trap — live in `~/.claude/learnings/agterm.md`. Read that before touching anything on agterm's side.

Related: [[context_percent_tokens_watcher_only]], [[terminal_title_followups]].
