---
layout: default
title: Install
parent: Home
nav_order: 1
---

Two steps: install the widget, then point Claude Code at the lifecycle hook.

## 1. Install the widget

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases) and run it.

The build is not yet code-signed on either platform, so you might see an OS warning on first launch.

### Windows

- File: `Claude Code Dashboard_<version>_x64-setup.exe`
- Requirements: Windows 10 version 1803 or newer.
- WebView2 is fetched automatically during install if missing.
- On first launch SmartScreen shows *"Windows protected your PC"* — click **More info** → **Run anyway**.

### macOS

- File: `Claude Code Dashboard_<version>_aarch64.dmg`
- Requirements: macOS 11+ on Apple Silicon.
- On first launch right-click the app → **Open** to bypass Gatekeeper.

After install, the widget lives in the system tray — left-click the tray icon to show or hide it. Until you wire the hook in step 2, no sessions will appear.

## 2. Wire the Claude Code hook

The dashboard integrates via [Claude Code lifecycle hooks](https://docs.claude.com/en/docs/claude-code/hooks) — Claude Code fires named events at specific moments during a session, and a small Python script (`integrations/claude_hook.py`, distributed with the widget source) turns each event into a status update for the widget.

Copy that script to a known location and point Claude Code at it from `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart":     [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "Notification":     [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "Stop":             [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "SessionEnd":       [{"hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]}],
    "PreToolUse": [{
      "matcher": "^(AskUserQuestion|ExitPlanMode)$",
      "hooks": [{"type": "command", "command": "python3 <repo>/integrations/claude_hook.py"}]
    }]
  }
}
```

Replace `<repo>` with the absolute path to your clone of this repo. Restart Claude Code — new sessions will appear in the widget as soon as you start one.

The `PreToolUse` matcher restricts the hook to user-gating tools (`AskUserQuestion`, `ExitPlanMode`). Claude Code buffers their `tool_use` blocks until the user answers, so without this hook the dashboard can't detect those calls in flight. The matcher also avoids per-Bash/Read/Grep fork overhead an unfiltered `PreToolUse` would incur.

## Optional tweaks

- **`projects_root`** in `config.json` — set to the folder your projects live under, so session ids become short folder-relative names instead of bare folder basenames. See [Features → session identity](features#session-identity).
- **`TAURI_DASHBOARD_URL`** environment variable — to use a non-default port or host, export e.g. `TAURI_DASHBOARD_URL=http://127.0.0.1:9100` before launching Claude Code. The hook resolves its URL from this variable, falling back to `http://127.0.0.1:9077`.

## Next

See [Features](features) for what each row shows once sessions start flowing in, and [Settings](settings) for the full `config.json` reference.
