---
name: debug-terminal-reading-e2e
description: E2e-test terminal-console-reading features by binding a synthetic Working session to the real terminal's console pids and POSTing directly to /api/event
metadata:
  type: project
---

To verify a feature that *reads the terminal console* (e.g. `idle_probe`'s cancelled-turn detection) end-to-end, you need a `Working` row bound to a console whose screen you control — the synthetic-hook path in [[debug-synthetic-hook-events]] isn't enough, because `claude_hook.py` reports the **Bash tool's hidden console**, not the real terminal (see [[debug-console-titles-tool-consoles]]).

Recipe (used 2026-06-06 to verify cancelled-turn demotion):

1. Get the **real** terminal's console pids from the **PowerShell tool** (which shares the real console): `GetConsoleProcessList`. These are the `console_pids` the hook would report for this session.
2. POST a synthetic `UserPromptSubmit` **directly** to `http://127.0.0.1:9077/api/event` with `client":"claude"`, a fake `cwd`/`session_id`, and `console_pids` set to those pids. The row goes `Working`, bound to your console. (A nonexistent `transcript_path` keeps the watcher a no-op so the row stays `Working`.)
3. Start a **background** watcher (PowerShell tool, `run_in_background`) tailing `widget.jsonl` for the demote log line — it survives across the turn boundary and notifies on completion.
4. **End the turn.** The terminal returns to Claude's idle prompt; the probe reads it and demotes. (While *you* are generating, the screen shows `esc to interrupt` → busy; you must end the turn for the idle state to appear.)
5. `SessionEnd` the synthetic id to clean up; blank the tab title.

To capture what the **idle** screen actually looks like (e.g. to pick the right idle anchor), launch a background PowerShell that `Start-Sleep`s ~12s, then reads the console and writes the tail to a temp file — the read lands after the turn ends, while the console is genuinely idle. A running background tool does **not** inject `esc to interrupt` into the footer, so it doesn't contaminate busy/idle classification.

**Why:** the read path only sees the real terminal when bound to its actual console pids, and the relevant screen states (idle vs generating) only exist across turn boundaries — so the test must straddle a turn end with a background observer. See `windows-console-screen-read.md` (global learning) for the read mechanics.
