---
name: debug-console-titles-tool-consoles
description: When verifying terminal-title behavior, the Bash tool runs in its own hidden conPTY console while the PowerShell tool shares the user's real terminal console
metadata:
  type: project
---

Claude Code's Bash tool spawns each command into a **separate hidden conPTY console** — `GetConsoleProcessList` there returns only the transient bash/python pids, never claude.exe. The PowerShell tool's persistent pwsh host is attached to the **user's real terminal console** (its process list includes claude.exe, the launching cmd wrapper, and the user's shell).

**Why:** Title writes verified by reading `GetConsoleTitleW` from a Bash-tool python looked like failures when they had actually landed on the user's tab (and vice versa — early "successful" tests had set the hidden Bash console's title). This burned an hour of false debugging during the terminal-tab-titles feature (2026-06-04).

**How to apply:** Any console-level verification (titles, `GetConsoleProcessList`, `AttachConsole` targets) must run via the PowerShell tool, not the Bash tool. Hook-spawn simulation (`CREATE_NO_WINDOW`) behaves the same from either. See [[debug-history-rendering-via-prompt-history]] for the analogous data-vs-render split.

**macOS variant (2026-06-04):** the Bash tool's shell has **no controlling tty** (`ps -o tty= -p $$` → `??`) — the same isolation in tty form. Resolve the real tab's tty from ancestor pids (the session's `claude` process, 1–2 levels up the chain), never from the tool/hook process itself.
