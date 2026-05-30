---
layout: default
title: Settings
parent: Home
nav_order: 3
---

All settings live in `config.json` under the app data directory:

- **Windows**: `%APPDATA%\com.anothersava.claude-code-dashboard\`
- **macOS**: `~/Library/Application Support/com.anothersava.claude-code-dashboard/`
- **Linux**: `$XDG_CONFIG_HOME/com.anothersava.claude-code-dashboard/` (or `~/.config/...`)

The tray menu has an **Open config/logs location** shortcut that opens the folder. The widget hot-reloads `config.json` on save — no restart needed except when changing `server_port`.

## Tray menu

Quick toggles that mirror specific `config.json` fields, plus shortcuts:

- **Show / Hide widget** — left-click toggle on the tray icon.
- **Always on top** — `always_on_top`.
- **Save position on exit** — `save_window_position`.
- **Start with system** — autostart via the OS (no `config.json` field; managed by Tauri's autostart plugin).
- **Auto-resize** submenu — Off / Up / Down (`auto_resize`).
- **History font size** submenu — five sizes (`history_font_size`).
- **Open config/logs location** — opens the app data directory.
- **Quit** — closes the widget.

## Top-level fields

| Field | Type | Default | Notes |
|---|---|---|---|
| `server_port` | int | `9077` | HTTP server port. **Requires restart** — not hot-reloaded. |
| `always_on_top` | bool | `true` | Whether the widget stays above other windows. |
| `save_window_position` | bool | `true` | Persist `window_position` / `history_window_position` on close (position **and** size). |
| `window_position` | `{x, y, width?, height?}` | `null` | Last-saved main window position and size. `width`/`height` are optional so older configs keep working. |
| `history_window_position` | `{x, y, width?, height?}` | `null` | Last-saved history window position and size. |
| `auto_resize` | enum | `"none"` | `"none"` / `"up"` / `"down"` — auto-fit window height to content; `up` keeps the bottom edge fixed, `down` keeps the top edge fixed. |
| `history_font_size` | enum | `"regular"` | `"smallest"` / `"small"` / `"regular"` / `"large"` / `"largest"`. |
| `limit_bar_segments` | int | `16` | Number of segments in the 5h / 7d usage limit bars. |
| `usage_limits_poll_interval_seconds` | int | `600` | How often to poll Anthropic's `/api/oauth/usage`. Clamped to 60s minimum at runtime. |
| `projects_root` | string | `null` | Folder your projects live under — turns session ids into short folder-relative names instead of bare basenames. |

## Token coloring

| Field | Type | Default |
|---|---|---|
| `context_window_tokens` | `{model: tokens}` | `{ "claude-opus-4-7": 1000000, "claude-sonnet-4-6": 200000, "claude-haiku-4-5": 200000 }` |
| `context_bar_thresholds` | `[{percent, color}]` | green at 0%, amber at 60%, red at 85% |

The widget interpolates the token-counter color from `context_bar_thresholds` based on the live token count as a percentage of the active model's `context_window_tokens` entry.

## Prompt classification

| Field | Type | Default | Notes |
|---|---|---|---|
| `benign_closers` | `[string]` | `["What's next?", "Anything else?"]` | Polite trailing questions that end with `?` but shouldn't flip the row to WAIT. Case-insensitive suffix match. |
| `continuation_prompts` | `[string]` | `["go", "continue", "proceed"]` | Replies that look like new prompts but are really *"keep going"* — suppress the DONE/IDLE → WORK task boundary, preserve the original prompt and timer. Exact match after trim, case-insensitive. |

For the full classification logic see [Classification](development/classification) and [Sticky labels](development/sticky-labels) in the Development section.

## Notifications

```json
{
  "notifications": {
    "telegram": {
      "bot_token": "<from @BotFather>",
      "chat_id": "<your chat id>",
      "state_thresholds_ms": {
        "awaiting": 60000,
        "error": 60000
      }
    }
  }
}
```

`state_thresholds_ms` keys: `"idle"`, `"working"`, `"awaiting"`, `"done"`, `"error"`. A missing key (or value `0`) means silent for that state. A session that stays in the named state for the threshold duration triggers a Telegram message.

To disable notifications entirely, set `"notifications": null` (or omit the object).
