---
layout: default
title: Settings
parent: Home
nav_order: 3
---

Every setting lives in a single `config.json` file. The tray menu is a shortcut for the handful of options you change most often — flipping a toggle there writes the matching `config.json` field — but `config.json` is the full surface, including settings with no tray equivalent.

Location of the `config.json` file under the app data directory:

- **Windows**: `%APPDATA%\com.anothersava.claude-code-dashboard\`
- **macOS**: `~/Library/Application Support/com.anothersava.claude-code-dashboard/`

The tray's **Open config/logs location** shortcut opens this folder. The widget hot-reloads `config.json` on save — no restart needed except when changing `server_port`.

## Tray menu

Right-click the tray icon for the controls you reach for most:

- **Show / Hide widget** — toggle the window (also a left-click on the tray icon).
- **Always on top** — keep the widget above your other windows.
- **Save position on exit** — reopen where you left it, at the same size.
- **Terminal tab titles** — show each session's status as a colored circle in its terminal tab title.
- **On system start** — launch at login: off, open the window, or start hidden in the tray.
- **Auto-resize** — fit the window height to its content, growing upward or downward.
- **History font size** — pick one of five sizes for the history window.
- **Open config/logs location** — open the app data folder.
- **Quit** — close the widget.

## Config file

Every field is optional — omit one and the built-in default applies. A complete config with those defaults, roughly ordered from the settings you're most likely to change to the ones you'll rarely touch:

```json
{
  "always_on_top": true,
  "history_font_size": "regular",
  "auto_resize": "none",
  "terminal_titles": true,
  "save_window_position": true,
  "window_position": null,
  "history_window_position": null,
  "start_minimized": false,
  "projects_root": null,
  "notifications": {
    "telegram": {
      "bot_token": null,
      "chat_id": null,
      "state_thresholds_ms": { "awaiting": 120000, "error": 60000 },
      "context_alert_percent": 80
    }
  },
  "context_bar_thresholds": [
    { "percent": 0,  "color": "#3a7c4a" },
    { "percent": 60, "color": "#c6a03c" },
    { "percent": 85, "color": "#c64a4a" }
  ],
  "context_window_tokens": {
    "claude-opus-4-7": 1000000,
    "claude-sonnet-4-6": 200000,
    "claude-haiku-4-5": 200000
  },
  "benign_closers": ["What's next?", "Anything else?"],
  "continuation_prompts": ["go", "continue", "proceed"],
  "limit_bar_segments": 16,
  "usage_limits_poll_interval_seconds": 600,
  "server_port": 9077
}
```

### Window and startup

Whether autostart is enabled isn't a config field — it lives in the OS launch entry (Windows registry / macOS LaunchAgent), so the tray's **On system start** submenu is the way to turn it on or off. `config.json` only persists `start_minimized`, the "open to tray" vs "open window" distinction.

- `always_on_top` — keep the widget above other windows.
- `history_font_size` — history-window text size, one of `"smallest"`, `"small"`, `"regular"`, `"large"`, `"largest"`.
- `auto_resize` — fit the window height to its content: `"up"` grows from a fixed bottom edge, `"down"` from a fixed top edge, `"none"` leaves the window manually sized.
- `save_window_position` — remember each window's position and size on close. The saved geometry lives in `window_position` and `history_window_position`, which the widget manages for you — no need to edit them by hand.
- `start_minimized` — when launched at login, stay hidden in the tray. Set it through the tray's **On system start → Open to tray**; it's ignored on a manual launch.

### Session identity

- `projects_root` — the folder your projects live under. Sessions beneath it get short, folder-relative names instead of bare folder basenames. See [Features → session identity](features#session-identity).

### Terminal tab titles

- `terminal_titles` — mirror each session's status onto its terminal tab as a colored circle next to the session name (🔵 working, 🟠 waiting, 🟢 done, 🔴 error, ⚪ idle). Windows only for now. See [Features → terminal tab titles](features#terminal-tab-titles).

### Notifications

The `notifications` block controls alerts when a session needs you. Set it to `null` (or omit it) to turn notifications off entirely. Telegram is the only channel today:

- `bot_token` / `chat_id` — your Telegram bot credentials; get `bot_token` from [@BotFather](https://t.me/BotFather).
- `state_thresholds_ms` — how long a session must sit in a state before it alerts, in milliseconds, keyed by state: `"idle"`, `"working"`, `"awaiting"`, `"done"`, `"error"`. A missing key or `0` keeps that state silent.
- `context_alert_percent` — send a one-off message when a session's context usage crosses this percent of the active model's window (the same percentage that colors the token counter). It fires once on crossing and re-arms only after usage drops back below — so a new task or `/clear` lets it alert again. `null` or `0` turns it off.

### Token coloring

- `context_bar_thresholds` — color stops for the token counter, each a `percent` and a hex `color`. The widget interpolates the color from the live count as a percentage of the active model's window — so it ramps green → amber → red as context fills.
- `context_window_tokens` — per-model context-window size, used as the denominator for that percentage. Add an entry for any model you use.

### Prompt classification

- `benign_closers` — polite trailing questions that end in `?` but shouldn't flip a finished row to WAIT. Matched case-insensitively as a suffix.
- `continuation_prompts` — short replies that mean *keep going* rather than a new task, so the original prompt and work timer carry over instead of resetting. Matched exactly, case-insensitively, after trimming.

For the full classification logic see [Classification](development/classification) and [Sticky labels](development/sticky-labels) in the Development section.

### Usage limit bars

- `limit_bar_segments` — number of segments in the 5-hour / 7-day usage bars; higher is finer-grained.
- `usage_limits_poll_interval_seconds` — how often to poll Anthropic for usage. Clamped to a 60-second minimum.

### Server port

- `server_port` — port the embedded HTTP server listens on for hook events. Most users never touch this. Two caveats if you do: it's the only setting that needs an app restart to take effect (everything else hot-reloads), and the Claude hook must point at the same port — it defaults to `9077`, so set `TAURI_DASHBOARD_URL=http://127.0.0.1:<port>` in the hook's environment to match.
