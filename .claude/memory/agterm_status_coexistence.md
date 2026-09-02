---
name: agterm-status-coexistence
description: This dashboard is the status source of truth inside agterm; agterm's own glyph is deliberately dark
metadata:
  type: project
---

On macOS the user's terminal is **agterm**, which has its own agent-status glyph fed by Claude Code hooks it installs into `~/.claude/settings.json`. As of 2026-08-27 those hooks are **removed**: this dashboard owns status display, and agterm's glyph stays idle.

**Why:** agterm's four values (idle/active/completed/blocked) cannot express `Waiting` or `Error`, so the two *status* readouts could never agree — and two status signals on one row is a duplicate state signal. What the user gives up (attention navigation, the blocked sound, auto-follow) was already unused. Status display stays single-source: the OSC 0 title.

**But agterm is still asked one question, because it is the only thing that can answer it.** "Has the user looked at this finished session yet" is orthogonal to `status` — both cases are `Done`, with the same `state_entered_at` and the same dialog — and `idle.rs` measures system-wide input, not per-session attention. agterm knows which session is selected and how long ago its window was touched, so `attention.rs` *reads* `tree --json`. The signal is a **selection change** between polls — opening a tab to read it is the act, and reading produces no input, so `idleMs` alone would leave a row unread exactly while it was being read — credited to the previous poll (the earliest the switch could have happened). `idleMs` is only the fallback for an already-selected session, stamped `poll_start - idleMs` (the observed last-input instant, never `now`). That is a read, not a second status path: nothing is written to agterm. A read finished row is then **shown as `Idle`** (`commands::apply_read_as_idle`), which is what makes agterm's tab distinguish read from unread — ⚪ vs 🟢 via the existing OSC 0 title — with no glyph write and no new marker.

**How to apply:**
- `terminal_title.rs`'s OSC 0 string *becomes* agterm's session name — that is the intended display here, not a side effect. Setting `terminal_titles: false` removes the dashboard from agterm entirely. `attention::resolve_row` reads that same title back to name the row, preferring it over the session's cwd: a row's id is cwd-derived only as a first-seen anchor, so a session that has `cd`-ed reports a cwd deriving *another* row's id.
- Do not **write** a second status path (e.g. calling `agtermctl session status` from `notifications.rs`) — agterm's docs treat the two sources as alternatives, not layers. Reading `tree --json` is not that, and `session_launcher.rs` already writes `session new`.
- Never read an absence as attention. agterm's indicator is ephemeral and unpersisted, so a restart wipes every mark at once and "no glyph" cannot mean "the user looked". Every attention source is a positive observation; a missing signal means unread, which shows the user something rather than hiding it.
- The glyph write (`session status completed --auto-reset`) is **not built and no longer wanted**. It was deferred as reopening a decision the user made himself; the DONE→IDLE model then made it redundant outright, since the OSC 0 title already distinguishes unread (🟢) from read (⚪) on the same row. A glyph carrying the same fact would be the duplicate state signal this file exists to prevent. Do not resurrect it as "the obvious next step".
- Keep this even though nothing uses it any more: had it been built, `AppStore.setAgentIndicator` assigns the indicator *before* its equality guard, so re-pushing `completed` after `--auto-reset` cleared it resurrects a glyph the user just dismissed. That ordering is deliberate on agterm's side (`statusChangedAt` is stamped on every non-idle write so "now minus this" means last-*written*), so it is not a bug to be fixed there — the hazard belongs to any writer.
- The agterm-side mechanics — OSC-title precedence, the un-re-runnable hook installer, the ghostty `title` shell-integration trap — live in `~/.claude/learnings/agterm.md`. Read that before touching anything on agterm's side.

Related: [[context_percent_tokens_watcher_only]], [[terminal_title_followups]], [[notification-delivery-channel]].
