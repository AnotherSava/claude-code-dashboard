---
layout: default
title: Home
nav_order: 1
---

*An always-on-top desktop widget that tracks your Claude Code sessions in real time.*

Each session appears as a row in a compact window with a state pill (WORK / WAIT / IDLE / DONE / ERROR), a live timer, and a token counter colored by how close the session is to its context limit. Integrates via [lifecycle hooks](pages/claude-code) — a thin Python script turns each Claude Code event into a status update for the widget.

![Claude Code Dashboard](screenshots/screenshot.png)

## Install

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases) and run it:

- **Windows**: `Claude Code Dashboard_<version>_x64-setup.exe` — Windows 10 version 1803 or newer; WebView2 is fetched automatically during install if missing.
- **macOS**: `Claude Code Dashboard_<version>_aarch64.dmg` — macOS 11+ on Apple Silicon. The build is not yet code-signed, so on first launch right-click the app → **Open** to bypass Gatekeeper.

## Setup

Follow the [Claude Code integration guide](pages/claude-code) to wire the hook into `~/.claude/settings.json`. New sessions will appear in the widget as soon as you start Claude Code.

## Usage

1. Launch the widget — it lives in the system tray; left-click the tray icon to show or hide the window.
2. Start a Claude Code session — the first hook event creates a row, status transitions animate the pill, and the row disappears when the session ends.

## Settings

All settings live in `config.json` under the app data directory — `%APPDATA%\com.anothersava.claude-code-dashboard\` on Windows, `~/Library/Application Support/com.anothersava.claude-code-dashboard/` on macOS. The tray menu has an "Open config/logs location" shortcut that opens the folder, and the widget hot-reloads `config.json` on save — no restart needed except when changing the HTTP server port.
