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

**Side effect (2026-06-04):** because the hooks run the **repo copy** of `claude_hook.py`, hook edits take effect on the next lifecycle event with no deploy. The app-data copy written by `setup.rs` on every launch only feeds the SetupPanel snippet for new users — it is not what this machine's hooks execute.
