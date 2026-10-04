---
created: 2026-10-03 16:42:46
---

# Take agterm's and agwinterm's agent-status hooks as a second status signal, rather than removing them

Both terminals ship their own agent-status hooks, which install Claude Code hooks that write a
status into the terminal directly. On this machine they were removed by hand, because the dashboard
owns the status and publishes it through the OSC title — two writers to one indicator is the
conflict `.claude/memory/agterm_status_coexistence.md` records.

Removing them throws away a signal. The proposal is to keep them installed but strip them of
control: they stop writing the terminal's own indicator and instead report to the dashboard, which
weighs what they say alongside the hook stream it already reads and stays the only writer of the
status. A second, independently-derived opinion about the same session is worth having wherever the
dashboard's own classification is uncertain — a BLOCK in particular, which is the state this repo
has spent the most effort inferring.

## What they install today

agterm's table is `claudeHooks` in `agtermCore/Sources/agtermCore/AgentHooksInstall.swift`, merged
into the user's `~/.claude/settings.json`, each entry invoking the wrapper `agterm-agent-status.sh`,
which calls `agtermctl session status`:

| Event | Matcher | State |
|---|---|---|
| `UserPromptSubmit` | — | `active --blink` |
| `PostToolUse` | — | `active --blink` |
| `Stop` | — | `completed --auto-reset` |
| `Notification` | `permission_prompt` | `blocked` |

agwinterm ships the Windows counterpart; find its equivalent table before designing anything, since
the two need not agree on events or states. agterm also installs adapters for Codex, OpenCode and
Pi, which the dashboard does not track at all — those are a separate question and probably the more
interesting half, since for a non-Claude agent this hook is the *only* status signal that exists.

## Why it might improve accuracy

The dashboard reads the same events, so the overlap is near-total and most of the value is in the
disagreements. Three worth checking before building anything, each measurable from `widget.jsonl`
against the terminal's own indicator:

- `Notification` with `permission_prompt` is a signal this repo deliberately **ignores**: it echoes
  a `PermissionRequest` that already named the tool, and carrying no `agent_id` even for a
  subagent's dialog it would relabel the row and could restore a BLOCK the release had just
  cleared. Whether it ever fires where no `PermissionRequest` did is unmeasured.
- `PostToolUse` is not consumed by this dashboard at all, and it is the first hook after a
  permission answer, which is a resume signal the current design infers from the transcript.
- `completed --auto-reset` clears on visit, which is read-ness — the same fact `attention.rs`
  derives, from a different oracle.

## Shape, if it goes ahead

Point the wrapper at `POST /api/event` rather than at `agtermctl session status`, or add a route
beside it, so the terminal's indicator stops being written by anyone but `terminal_title::sync`.
Treat what arrives as evidence and not as a status: the dashboard decides, logs the disagreement
with its own classification, and only then considers acting on it. Start by logging the two
opinions side by side for a week and reading how often they differ — a signal that never disagrees
is not worth wiring, and the disagreements are what say which direction the wiring should run.

Related: [[agterm_status_coexistence]].
