---
name: terminal-title-followups
description: Deferred follow-ups from the 2026-06-04 terminal tab titles feature — "yes" button via WriteConsoleInputW, Claude's own OSC title clobbering
metadata:
  type: project
---

Agreed during the terminal tab titles feature (2026-06-04), deliberately deferred:

- **"Yes" button**: user wants a dashboard button that answers Claude prompts in the terminal. Mechanism: while attached to the session's console, open `CONIN$` and inject keystrokes via `WriteConsoleInputW` (works without window focus). Needs a staleness guard (only while `Awaiting`, verify recency) and per-prompt-type keys (permission prompts take `1`/`2`/Enter; AskUserQuestion takes arrows+Enter). Reuses the pid-candidate infrastructure in `terminal_title.rs`.
- **Title fights with Claude Code itself**: claude.exe emits OSC 2 title sequences on render ticks (see the global `windows-terminal-title` learning) — these bypass conhost so `GetConsoleTitleW` can't observe them, and they can clobber the circle on the user's tab while Claude renders. The 5s reassert only heals during active emits (watcher activity). If the user reports circles vanishing on quiet sessions, options: reassert from the 1s notifications tick, or document `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1`. On this machine the fight can't happen: the `claude()` wrapper in `~/.zshrc` already exports that env var (see the dotfiles repo's `shell-environment.md` learning) — the caveat only applies to other users' setups.

**Why:** these are design decisions and known gaps not visible in the code; the feature looks "done" but has agreed next steps.

**How to apply:** when the user revisits terminal titles or the yes-button idea, start from these mechanisms instead of re-deriving them. See [[debug-console-titles-tool-consoles]] for the verification trap. (macOS support shipped 2026-06-04: hook reports the ancestor pid chain, `push_title`'s unix arm resolves the controlling tty via `ps -o tty=` and writes an OSC 0 escape.)
