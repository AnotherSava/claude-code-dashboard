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
- **Color terminal tabs** — show each session's status as a colored circle in its terminal tab title.
- **On system start** — launch at login: off, open the window, or start hidden in the tray.
- **Auto-resize** — fit the window height to its content, growing upward or downward.
- **History font size** — pick one of five sizes for the history window.
- **Tray usage badge** — show the 5-hour or 7-day usage on the tray icon, as the recolored traffic light or a number (or none).
- **Open config/logs location** — open the app data folder.
- **Quit** — close the widget.

## Config file

Every field is optional — omit one and the built-in default applies. A complete config with those defaults, roughly ordered from the settings you're most likely to change to the ones you'll rarely touch:

```json
{
  "always_on_top": true,
  "history_font_size": "regular",
  "auto_resize": "none",
  "tray_badge": "none",
  "terminal_titles": true,
  "detect_cancelled_turns": true,
  "save_window_position": true,
  "window_position": null,
  "history_window_position": null,
  "history_window_maximized": false,
  "start_minimized": false,
  "projects_root": null,
  "notifications": {
    "telegram": {
      "bot_token": null,
      "chat_id": null,
      "states": {
        "done":     { "afk_window_ms": 60000 },
        "awaiting": { "afk_window_ms": 60000, "reaction_window_ms": 120000 },
        "error":    { "afk_window_ms": 60000, "reaction_window_ms": 60000 }
      },
      "context_alert_percent": 80
    }
  },
  "context_bar_thresholds": [
    { "percent": 0,  "color": "#3a7c4a" },
    { "percent": 60, "color": "#c6a03c" },
    { "percent": 85, "color": "#c64a4a" }
  ],
  "context_window_tokens": {
    "claude-opus": 1000000,
    "claude": 200000
  },
  "benign_closers": ["What's next?"],
  "benign_openers": ["anything"],
  "continuation_prompts": ["go", "continue", "proceed", "yes", "y", "yeah", "yep", "yup", "ok", "okay", "sure", "go ahead", "do it"],
  "limit_bar_segments": 16,
  "usage_limits_poll_interval_seconds": 600,
  "sync": {
    "device_name": "",
    "listen": false,
    "listen_port": 9078,
    "peers": [],
    "token": null
  },
  "server_port": 9077
}
```

### Window and startup

Whether autostart is enabled isn't a config field — it lives in the OS launch entry (Windows registry / macOS LaunchAgent), so the tray's **On system start** submenu is the way to turn it on or off. `config.json` only persists `start_minimized`, the "open to tray" vs "open window" distinction.

- `always_on_top` — keep the widget above other windows.
- `history_font_size` — history-window text size, one of `"smallest"`, `"small"`, `"regular"`, `"large"`, `"largest"`.
- `auto_resize` — fit the window height to its content: `"up"` grows from a fixed bottom edge, `"down"` from a fixed top edge, `"none"` leaves the window manually sized.
- `save_window_position` — remember each window's position and size on close. The saved geometry lives in `window_position`, `history_window_position`, and `history_window_maximized`, which the widget manages for you — no need to edit them by hand. If the monitor a window was saved on is later disconnected or rearranged, the widget pulls it back onto a visible screen so it can't get stranded off-screen.
- `start_minimized` — when launched at login, stay hidden in the tray. Set it through the tray's **On system start → Open to tray**; it's ignored on a manual launch.

### Session identity

- `projects_root` — the folder your projects live under. Sessions beneath it get short, folder-relative names instead of bare folder basenames. See [Features → session identity](features#session-identity).

### Color terminal tabs

- `terminal_titles` — mirror each session's status onto its terminal tab as a colored circle next to the session name (🔵 working, 🟠 waiting, 🟢 done, 🔴 error, ⚪ idle). See [Features → color terminal tabs](features#color-terminal-tabs).

### Behavior

- `detect_cancelled_turns` — settle a working row back out of the working state when its turn was cancelled with Esc, returning it to wherever it was before that turn — a question it was waiting on, otherwise idle — so a cancelled reply doesn't get mistaken for a new task. Cancelling emits no event; the dashboard recognizes it from the conversation transcript, and on Windows also by noticing the terminal returned to its idle prompt (which catches an instant cancel that left nothing in the transcript). On by default. Turn it off to leave a cancelled row showing as working until the next prompt.

### Notifications

The `notifications` block controls alerts when a session needs you. Set it to `null` (or omit it) to turn notifications off entirely. Telegram is the only channel today:

- `bot_token` / `chat_id` — your Telegram bot credentials; get `bot_token` from [@BotFather](https://t.me/BotFather).
- `states` — per-state notification rules, keyed by state (`"idle"`, `"working"`, `"awaiting"`, `"done"`, `"error"`). Each state takes two independent, optional windows in milliseconds, and alerts as soon as *either* is met:
  - `afk_window_ms` — alert once you've been away from the keyboard/mouse this long *and* haven't touched the machine since the state began. This is the "you stepped away and missed it" trigger: if you were at the machine when it happened, it stays silent (you saw it). Omit it to disable the away-detection for that state.
  - `reaction_window_ms` — alert once the state has lasted this long regardless of whether you're present — the backstop for something you need to act on but haven't. Omit it for no backstop.
  - A state with neither window set, or a missing state key, never alerts. By default `done` is away-only (no backstop — a finished task you saw needs no nag), while `awaiting` and `error` also carry a reaction backstop.
- `context_alert_percent` — send a one-off message when a session's context usage crosses this percent of the active model's window (the same percentage that colors the token counter). It fires once on crossing and re-arms only after usage drops back below — so a new task or `/clear` lets it alert again. `null` or `0` turns it off.

### Token coloring

- `context_bar_thresholds` — color stops for the token counter, each a `percent` and a hex `color`. The widget interpolates the color from the live count as a percentage of the active model's window — so it ramps green → amber → red as context fills.
- `context_window_tokens` — context-window size per model, used as the denominator for that percentage. Keys match by longest prefix, so the two defaults above cover every Claude model — add an exact model id (e.g. `"claude-sonnet-4-6": 1000000`) to override its family.

### Prompt classification

- `benign_closers` — polite trailing questions that end in `?` but shouldn't flip a finished row to WAIT. Matched case-insensitively as a suffix.
- `benign_openers` — words that, when they open the final question, mark it an optional offer rather than a hand-back, so a sign-off like "Anything you'd like to look at?" stays DONE. Matched case-insensitively as a prefix of the last sentence. An embedded real ask still flips to WAIT, so "Anything else, or shall I commit?" still waits. Default: `["anything"]`.
- `continuation_prompts` — short replies that mean *keep going* or *yes, go ahead* rather than a new task (the defaults cover `go` / `continue` / `proceed` plus approvals like `yes` / `y` / `ok` / `sure`), so the original prompt and work timer carry over instead of resetting. Matched exactly, case-insensitively, after trimming.

For the full classification logic see [Classification](development/classification) and [Sticky labels](development/sticky-labels) in the Development section.

### Usage limit bars

- `limit_bar_segments` — number of segments in the 5-hour / 7-day usage bars; higher is finer-grained.
- `usage_limits_poll_interval_seconds` — how often to poll Anthropic for usage. Clamped to a 60-second minimum.
- `tray_badge` — show a usage limit on the tray icon. Values: `"none"`; `"five_hour_light"` / `"seven_day_light"` (recolor the traffic-light icon by usage); `"five_hour_number"` / `"seven_day_number"` (show the percentage, all-red light at 100%). The hover tooltip always shows both figures. Set it from the tray's **Tray usage badge** submenu.

### Multi-device sync

The `sync` block shows sessions from your other computers — see [Features → multi-device sync](features#multi-device-sync). The devices must be able to reach each other by address; across different networks the simplest way is a VPN like [Tailscale](https://tailscale.com/). On each device, point `peers` at the other devices, turn `listen` on, and set the same `token` everywhere:

- `device_name` — the name other dashboards show on this device's session badges. Filled in from the computer's hostname on first launch; edit it if you'd like a friendlier label.
- `listen` — accept session pushes from peers. Needs an app restart to change, like `server_port`.
- `listen_port` — port the sync listener uses (peers connect here). Also restart-required.
- `peers` — addresses of the other devices' sync listeners, e.g. `["http://my-laptop:9078"]`.
- `token` — a shared secret, the same string on every device; pushes without it are rejected. Sync stays fully off while it's `null`.

A typical two-device setup — desktop: `"listen": true, "peers": ["http://laptop:9078"], "token": "pick-a-long-random-string"`; laptop: the same with `"peers": ["http://desktop:9078"]`.

### Server port

- `server_port` — port the embedded HTTP server listens on for hook events. Most users never touch this. Two caveats if you do: it needs an app restart to take effect (like the `sync` listener settings, while everything else hot-reloads), and the Claude hook must point at the same port — it defaults to `9077`, so set `TAURI_DASHBOARD_URL=http://127.0.0.1:<port>` in the hook's environment to match.
