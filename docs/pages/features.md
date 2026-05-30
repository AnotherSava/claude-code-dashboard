---
layout: default
title: Features
parent: Home
nav_order: 2
---

What you see in the widget once Claude Code sessions are firing events at it.

## Session identity

Each Claude Code session becomes one row. The row's `id` is derived from the working directory of the session's **first** event — if `cwd` sits under the configured `projects_root`, the relative path becomes the id with slashes, dashes, and underscores replaced by spaces. Sessions outside `projects_root` fall back to the folder's base name; sessions with no `cwd` use the first eight characters of the Claude session id. The id is then locked to the session, so if the agent `cd`s into a subdirectory mid-conversation the row stays put rather than splitting into a second row.

**Renaming a row.** Double-click a row's name to edit it — Enter saves, Esc cancels, an empty value reverts to the derived id. The custom name is remembered per project, so a later Claude session in the same directory shows the same name.

## Live status

The row's state pill tracks the agent in real time:

- **WORK** — Claude is working on your task. Timer accumulates total time spent working on the same prompt across approval cycles.
- **WAIT** — Claude is blocked on you. The row shows the agent's current question or permission request.
- **IDLE** — the session is alive but not actively working.
- **DONE** — last turn ended without a question. Timer shows time since the session finished.
- **ERROR** — the hook reported an error; the label shows the error text.

## Sticky original prompt

During approval cycles — when Claude asks *"Can I run bash X?"* and waits for you to type *yes* — the row keeps displaying your **original task prompt** rather than the approval question or the *yes*. The pill still flips to WAIT so you see the agent is blocked, but the label reads what you actually asked for. The timer pauses during WAIT and resumes on the next WORK. A new top-level prompt after DONE / IDLE starts a fresh task boundary.

For the full state machine and the rules that pick between the current label and the original prompt, see [Sticky labels](development/sticky-labels) in the Development section.

## Live token count

When the hook provides a `transcript_path`, the widget tails the session's JSONL transcript and pulls the most recent assistant turn's input-side token count (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`). The token display is colored green → amber → red based on the configured thresholds relative to the model's context window — so you can tell at a glance whether `/compact` is due.

## History window

Double-click a session's name area (outside the rename trigger zone) to open a History window for that session — a chronological view of every user prompt, assistant reply, and `/clear` separator. Useful for scrolling back through a long-running conversation without leaving the dashboard.

Ctrl+`+` and Ctrl+`-` cycle through five font sizes; Esc closes the window. The choice persists to `config.json`.

## Window and tray

- Always-on-top tray-only window (no taskbar entry), draggable by the header strip; a hover-revealed × in the header hides it back to tray.
- System tray with show/hide toggle, always-on-top toggle, autostart toggle, save-position-on-exit toggle, open-config-file and open-log-file shortcuts, and a history font-size submenu.
- Color-coded state pills with a pulse animation on WAIT and ERROR.
- Transcript-based token tracking: each session's `.jsonl` is tailed in place; updates surface within milliseconds of an assistant turn being written.
- Sticky original-prompt label across approval cycles; the WORK timer treats a same-task approval round-trip as one continuous unit of work.
- Benign closers: configurable list of conversational closers (e.g. *"What's next?"*) that end with `?` but shouldn't flip the session into WAIT.
- Session-scoped watchers: each session gets its own filesystem watcher; `clear` events tear it down so idle sessions don't hold handles.
- Config hot-reload from `config.json` on the next save — except `server_port`, which requires a restart.

## Next

See [Settings](settings) for how to configure thresholds, benign closers, continuation prompts, Telegram notifications, and more.
