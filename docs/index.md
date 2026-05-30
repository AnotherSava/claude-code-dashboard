---
layout: default
title: Home
nav_order: 1
has_children: true
---

*An always-on-top desktop widget that tracks your Claude Code sessions in real time.*

![Claude Code Dashboard](screenshots/screenshot.png)

Each session appears as a row in a compact window with a state pill (WORK / WAIT / IDLE / DONE / ERROR), a live timer, and a token counter colored by how close the session is to its context limit. A thin Python hook turns each Claude Code lifecycle event into a status update for the widget.

## Next steps

- **[Install](pages/install)** — download the installer and wire the Claude Code lifecycle hook.
- **[Features](pages/features)** — what each row shows: status pills, sticky prompts, live tokens, renaming.
- **[Settings](pages/settings)** — all `config.json` options and the tray menu.
- **[Development](pages/development)** — building from source, architecture, and internals.
