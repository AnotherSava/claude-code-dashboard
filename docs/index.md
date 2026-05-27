---
layout: default
title: Home
nav_order: 1
---

*A real-time desktop widget that tracks what your AI coding agents are doing.*

Anything that can POST JSON to `localhost` can report status. Each session appears as a row in a compact always-on-top window, with a state pill that transitions between WORK / WAIT / IDLE / DONE / ERROR, a live timer, and a token counter colored by how close the session is to its context limit.

![Claude Code Dashboard](screenshots/screenshot.png)

## Install

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases) and run it:

- **Windows**: `Claude Code Dashboard_<version>_x64-setup.exe` — Windows 10 version 1803 or newer; WebView2 is fetched automatically during install if missing.
- **macOS**: `Claude Code Dashboard_<version>_aarch64.dmg` — macOS 11+ on Apple Silicon. The build is not yet code-signed, so on first launch right-click the app → **Open** to bypass Gatekeeper.

After installation, configure the agents you want to track:

## Claude Code

[First-class integration](pages/claude-code) via lifecycle hooks in `~/.claude/settings.json`. Each Claude Code session becomes a row named after its working directory, with state tracked through SessionStart / UserPromptSubmit / Notification / Stop / SessionEnd events. A transcript watcher tails each session's JSONL to update token counts live between hook firings.

## HTTP API

[A generic POST endpoint](pages/http-api) — any tool, language, or CI script that can send JSON can report status. A three-line curl is enough, and the payload format is the same as Claude Code's.

## Usage

1. Launch the widget — it lives in the system tray; left-click the tray icon to show or hide the window.
2. Wire your agent of choice by following the [Claude Code](pages/claude-code) or [HTTP API](pages/http-api) guide.
3. Start a session — the first POST creates a row, status transitions animate the pill, and `clear` removes the row when a session ends.

## Settings

All settings live in `config.json` under the app data directory — `%APPDATA%\com.anothersava.claude-code-dashboard\` on Windows, `~/Library/Application Support/com.anothersava.claude-code-dashboard/` on macOS. The tray menu has an "Open config/logs location" shortcut that opens the folder, and the widget hot-reloads `config.json` on save — no restart needed except when changing the HTTP server port.
