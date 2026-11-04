---
created: 2026-11-03 13:05:01
platform: macos
---

# Add a features.md figure for the dev-server line and its overlay

The section 'See and open a running dev server' in docs/pages/features.md (added 2026-11-03) has no figure. It would show an agterm sidebar row with the green '▶ dev server :<port>' line indented under the session name, and optionally the server's page open as an overlay over the session (cmd+shift+s). Stage it by announcing a throwaway local server through POST /api/dev-server with the session's AGTERM_SESSION_ID, so no real project or prompt appears in the frame, and capture through the docs-screenshot skill with DocShot. Taking it drives agterm, so ask right before the capture. Raised as gap G1 by screenshot-relevance during that day's /wrap-up.
