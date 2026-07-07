# Claude Code Dashboard

*A live monitor for your Claude Code sessions.*

A desktop app for Windows and macOS — a compact always-on-top widget that lists your agents with each one's current status and the task it's working on. The header tracks your 5-hour and 7-day Anthropic usage limits.

![Claude Code Dashboard](docs/screenshots/screenshot.png)

## Features

- **Notifications** — Telegram pings you when an agent finishes or is waiting while you're away from your machine — staying quiet when you were there to see it — when a session's context fills past a threshold, or when a heavily-used usage limit resets.
- **Color terminal tabs** — each session's status appears as a colored circle in its terminal tab — with its context usage once it climbs high — so you see who needs you even without the widget on screen.
- **Multi-device sync** — track sessions running on your other devices the same way as local ones.
- **Focus on the task** — once Claude resumes after a question, the row shows your original request, not the *yes* you typed.
- **Context usage** — each row shows how full the model's context is, colored green → amber → red as it fills, so you can tell at a glance whether `/compact` is due.
- **History window** — a recap of the work so far: your prompts and Claude's reply to each, with session boundaries marked.
- **Work intensity** — a separate window charting how hard your agents have been working over time, by day or by week, from your 5-hour usage history.

---

See full project documentation at **[anothersava.github.io/claude-code-dashboard](https://anothersava.github.io/claude-code-dashboard/)**:

- [Installation](https://anothersava.github.io/claude-code-dashboard/pages/install)
- [Features](https://anothersava.github.io/claude-code-dashboard/pages/features)
- [Settings](https://anothersava.github.io/claude-code-dashboard/pages/settings)
- [Development](https://anothersava.github.io/claude-code-dashboard/pages/development)
  - [Classification](https://anothersava.github.io/claude-code-dashboard/pages/development/classification)
  - [Sticky labels](https://anothersava.github.io/claude-code-dashboard/pages/development/sticky-labels)
  - [Data flow](https://anothersava.github.io/claude-code-dashboard/pages/development/data-flow)
  - [HTTP API](https://anothersava.github.io/claude-code-dashboard/pages/development/http-api)
