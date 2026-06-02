---
layout: default
title: Installation
parent: Home
nav_order: 1
---

Two steps: install the widget, then point Claude Code at the lifecycle hook.

## 1. Install the widget

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases) and run it.

### Windows

- File: `Claude Code Dashboard_<version>_x64-setup.exe`
- Requirements: Windows 10 version 1803 or newer.
- WebView2 is fetched automatically during install if missing.
- On first launch SmartScreen shows *"Windows protected your PC"* — click **More info** → **Run anyway**.

### macOS

- File: `Claude Code Dashboard_<version>_aarch64.dmg`
- Requirements: macOS 11+ on Apple Silicon.
- The build is ad-hoc signed but not Apple-notarized, so on first launch macOS will say *"damaged and can't be opened"*. Open **System Settings → Privacy & Security**, scroll to the blocked-app notice, and click **Open Anyway**. Power users can instead run `xattr -cr "/Applications/Claude Code Dashboard.app"` in Terminal once after install.

After install, the widget lives in the system tray — left-click the tray icon to show or hide it. Until you wire the hook in step 2, no sessions will appear.

## 2. Wire the Claude Code hook

The dashboard integrates via [Claude Code lifecycle hooks](https://docs.claude.com/en/docs/claude-code/hooks) — Claude Code fires named events at specific moments during a session, and a small Python script (`claude_hook.py`) turns each event into a status update for the widget. **You don't need to clone this repo:** the widget ships the script inside the binary and writes a fresh copy to its app-data folder on every launch.

### Copy the snippet from the widget (recommended)

Until it receives its first event, the widget shows an **Instructions to connect Claude Code** panel. It contains the exact `~/.claude/settings.json` snippet with the hook path and Python launcher **already filled in for your setup**.

1. Launch the widget (left-click the tray icon if it's hidden).
2. In the instructions panel, click **Copy**.
3. Open `~/.claude/settings.json` (create it if missing) and paste the snippet in. If the file already has a `"hooks"` block, merge the entries rather than overwriting.
4. Restart Claude Code, then start a session.

The panel disappears automatically once the dashboard receives its first event. Prefer to let Claude Code do the wiring? Ask it to follow the steps in the panel — it can edit `settings.json` for you.

### What the snippet looks like

For reference, the copied snippet has this shape (your copy substitutes the real absolute path to `claude_hook.py` in your app-data folder, and `python` vs `python3` for your platform):

```json
{
  "hooks": {
    "SessionStart":     [{"hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]}],
    "Notification":     [{"hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]}],
    "Stop":             [{"hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]}],
    "SessionEnd":       [{"hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]}],
    "PreToolUse": [{
      "matcher": "^(AskUserQuestion|ExitPlanMode)$",
      "hooks": [{"type": "command", "command": "python3 \"<app-data>/claude_hook.py\""}]
    }]
  }
}
```

The app-data folder is `%APPDATA%\com.anothersava.claude-code-dashboard\` on Windows and `~/Library/Application Support/com.anothersava.claude-code-dashboard/` on macOS. The instructions panel's **Hook script** line links straight to it.

The `PreToolUse` matcher restricts the hook to user-gating tools (`AskUserQuestion`, `ExitPlanMode`). Claude Code buffers their `tool_use` blocks until the user answers, so without this hook the dashboard can't detect those calls in flight. The matcher also avoids per-Bash/Read/Grep fork overhead an unfiltered `PreToolUse` would incur.

## Optional tweaks

- **`projects_root`** in `config.json` — set to the folder your projects live under, so session ids become short folder-relative names instead of bare folder basenames. See [Features → session identity](features#session-identity).
- **`TAURI_DASHBOARD_URL`** environment variable — to use a non-default port or host, export e.g. `TAURI_DASHBOARD_URL=http://127.0.0.1:9100` before launching Claude Code. The hook resolves its URL from this variable, falling back to `http://127.0.0.1:9077`.

## Next

See [Features](features) for what each row shows once sessions start flowing in, and [Settings](settings) for the full `config.json` reference.
