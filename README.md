# Claude Code Dashboard

*An always-on-top desktop widget that tracks your Claude Code sessions in real time.*

Each session appears as a row in a compact window with a state pill (WORK / WAIT / IDLE / DONE / ERROR), a live timer, and a token counter colored by how close the session is to its context limit. A thin Python hook turns each Claude Code lifecycle event into a status update for the widget — see [Install](https://anothersava.github.io/claude-code-dashboard/pages/install) for setup.

![Claude Code Dashboard](docs/screenshots/screenshot.png)

---

Download the latest installer for your platform from the [Releases page](https://github.com/AnotherSava/claude-code-dashboard/releases):

- **Windows**: `Claude Code Dashboard_<version>_x64-setup.exe` — Windows 10 version 1803 or newer; WebView2 is fetched during install if missing.
- **macOS**: `Claude Code Dashboard_<version>_aarch64.dmg` — macOS 11+ on Apple Silicon.

See full project documentation at **[anothersava.github.io/claude-code-dashboard](https://anothersava.github.io/claude-code-dashboard/)**:

- [Install](https://anothersava.github.io/claude-code-dashboard/pages/install)
- [Features](https://anothersava.github.io/claude-code-dashboard/pages/features)
- [Settings](https://anothersava.github.io/claude-code-dashboard/pages/settings)
- [Development](https://anothersava.github.io/claude-code-dashboard/pages/development)
  - [Classification](https://anothersava.github.io/claude-code-dashboard/pages/development/classification)
  - [Sticky labels](https://anothersava.github.io/claude-code-dashboard/pages/development/sticky-labels)
  - [Data flow](https://anothersava.github.io/claude-code-dashboard/pages/development/data-flow)
  - [HTTP API](https://anothersava.github.io/claude-code-dashboard/pages/development/http-api)
