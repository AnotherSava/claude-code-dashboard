---
name: Claude Code hooks require $CLAUDE_AI_AGENT_DASHBOARD
description: Hooks in ~/.claude/settings.json reference the project root via $CLAUDE_AI_AGENT_DASHBOARD env var; if unset, every Claude Code lifecycle event silently fails to reach the dashboard.
type: project
---

`~/.claude/settings.json` has 5 hook entries that all expand to:

    python3 "$CLAUDE_AI_AGENT_DASHBOARD/integrations/claude_hook.py" <verb>

If the env var is unset, the path expands to `/integrations/claude_hook.py` which doesn't exist — the hook fails silently (stdout/stderr aren't surfaced), no session ever appears in the dashboard.

**Setup per shell:**
- macOS (zsh): `export CLAUDE_AI_AGENT_DASHBOARD="$HOME/Projects/tauri-dashboard"` in `~/.zshrc`
- Windows (Git Bash or system env): same, with the right project path

**Why:** Keeps `~/.claude/settings.json` portable across machines that may clone the repo to different locations. Trade-off: silent failure when the var is unset; consider adding a stderr guard in `integrations/claude_hook.py` if the silent-failure mode keeps biting.

**How to apply:** When debugging "hooks aren't firing" or onboarding the project to a new machine, this is the first thing to check.
