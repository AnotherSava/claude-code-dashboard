---
layout: default
title: Claude Code
---

[Home](..) | [Claude Code](claude-code) | [HTTP API](http-api) | [Classification](classification) | [Sticky labels](sticky-labels) | [Data flow](data-flow) | [Development](development)

---

[Claude Code](https://docs.claude.com/en/docs/claude-code) is Anthropic's official command-line coding agent. The dashboard integrates via lifecycle hooks — Claude Code fires named events at specific moments during a session, and a small Python script turns each event into a status update for the widget.

### Session identity

Each Claude Code session becomes one row in the widget. The row's `id` is derived from the session's current working directory — if `cwd` sits under the configured `projects_root`, the relative path becomes the id with slashes, dashes, and underscores replaced by spaces. Sessions outside `projects_root` fall back to the folder's base name; sessions with no `cwd` use the first eight characters of the Claude session id.

### Live status

The row's state pill tracks the agent in real time:

- **WORK** — Claude is working on your task. Timer accumulates total time spent working on the same prompt across approval cycles.
- **WAIT** — Claude is blocked on you. The row shows the agent's current question or permission request.
- **IDLE** — the session is alive but not actively working.
- **DONE** — last turn ended without a question. Timer shows time since the session finished.
- **ERROR** — the hook reported an error; the label shows the error text.

### Sticky original prompt

During approval cycles — when Claude asks *"Can I run bash X?"* and waits for you to type *yes* — the row keeps displaying your **original task prompt** rather than the approval question or the *yes*. The pill still flips to WAIT so you see the agent is blocked, but the label reads what you actually asked for. The timer pauses during WAIT and resumes on the next WORK. A new top-level prompt after DONE / IDLE starts a fresh task boundary. See [Sticky labels](sticky-labels) for the full state machine and the rules that pick between the current label and the original prompt.

### Live token count

When the hook provides a `transcript_path`, the widget tails the session's JSONL transcript and pulls the most recent assistant turn's input-side token count (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`). The token display is colored green → amber → red based on the configured thresholds relative to the model's context window — so you can tell at a glance whether `/compact` is due.

### Setup

Install the widget and verify it's running: the tray icon should be visible.

Copy `integrations/claude_hook.py` — distributed with the widget source — and point Claude Code at it from `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart":      [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "UserPromptSubmit":  [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "Notification":      [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "Stop":              [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "SessionEnd":        [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "PreToolUse":        [{"matcher": "^(AskUserQuestion|ExitPlanMode)$", "hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}]
  }
}
```

Replace `<repo>` with the absolute path to your clone of this repo. Restart Claude Code — new sessions will appear in the widget.

The `PreToolUse` matcher restricts the hook to user-gating tools (`AskUserQuestion`, `ExitPlanMode`) — Claude Code buffers their `tool_use` blocks until the user answers, so without this hook the dashboard can't detect those calls in flight. The matcher also avoids per-Bash/Read/Grep fork overhead an unfiltered `PreToolUse` would incur.

Optional: set `projects_root` in `config.json` to the folder your projects live under, so session ids become short folder-relative names instead of bare folder basenames.

The hook talks to the widget at `http://127.0.0.1:9077` by default. To use a different port or host, export `TAURI_DASHBOARD_URL` (e.g. `TAURI_DASHBOARD_URL=http://127.0.0.1:9100`) before launching Claude Code.

### Features

- **Transcript-based token tracking**: each session's `.jsonl` is tailed in place; updates surface within milliseconds of an assistant turn being written.
- **Approval-cycle-aware label**: the visible label and the WORK timer both treat a same-task approval round-trip as one continuous unit of work.
- **Benign closers**: configurable list of conversational closers (e.g. *"What's next?"*) that end with `?` but shouldn't flip the session into WAIT.
- **Session-scoped watchers**: each session gets its own filesystem watcher; `clear` events tear it down so idle sessions don't hold handles.

### Standard features

- Always-on-top tray-only window (no taskbar entry), draggable by the header strip; a hover-revealed × in the header hides it back to tray.
- System tray with show/hide toggle, always-on-top toggle, autostart toggle, save-position-on-exit toggle, open-config-file and open-log-file shortcuts.
- Color-coded state pills with a pulse animation on WAIT and ERROR.
- Sticky original-prompt label across approval cycles; same trigger resets the WORK accumulator on a new task.
- Config hot-reload from `config.json` on the next save — except `server_port`, which requires a restart.
