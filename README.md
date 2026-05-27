# Claude Code Dashboard

*An always-on-top desktop widget that tracks your Claude Code sessions in real time.*

Each session appears as a row in a compact window with a state pill (WORK / WAIT / IDLE / DONE / ERROR), a live timer, and a token counter colored by how close the session is to its context limit. Integrates via [lifecycle hooks](https://anothersava.github.io/claude-code-dashboard/pages/claude-code) — a thin Python script turns each Claude Code event into a status update for the widget.

![Claude Code Dashboard](docs/screenshots/screenshot.png)

---

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases):

- **Windows**: `Claude Code Dashboard_<version>_x64-setup.exe` — Windows 10 version 1803 or newer; WebView2 is fetched during install if missing.
- **macOS**: `Claude Code Dashboard_<version>_aarch64.dmg` — macOS 11+ on Apple Silicon.

See full project documentation at **[anothersava.github.io/claude-code-dashboard](https://anothersava.github.io/claude-code-dashboard/)**:

- [Claude Code](https://anothersava.github.io/claude-code-dashboard/pages/claude-code)
- [Development](https://anothersava.github.io/claude-code-dashboard/pages/development)
  - [Classification](https://anothersava.github.io/claude-code-dashboard/pages/classification)
  - [Sticky labels](https://anothersava.github.io/claude-code-dashboard/pages/sticky-labels)
  - [Data flow](https://anothersava.github.io/claude-code-dashboard/pages/data-flow)
  - [HTTP API](https://anothersava.github.io/claude-code-dashboard/pages/http-api)
