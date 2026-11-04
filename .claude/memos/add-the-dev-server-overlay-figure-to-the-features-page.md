---
created: 2026-11-04 11:44:14
platform: macos
---

# Add the dev-server overlay figure to the features page, beside the sidebar line

Memo 3 (closed 2026-11-04) asked for two figures in docs/pages/features.md, section 'See and open a running dev server': the green dev-server line in agterm's sidebar, which is now docs/screenshots/dev-server-line-macos.png, and optionally the server's page open as an overlay over the session (cmd+shift+s, integrations/open-dev-server.py). The overlay was left out because shooting it from the working session would have shown that session's own conversation behind the overlay. Next step: stage a throwaway agterm session whose terminal holds nothing private, announce a throwaway http.server through POST /api/dev-server with that session's AGTERM_SESSION_ID (as docs/screenshots/capture/dev-server-line-macos.py does), open the overlay on it, and capture through the docs-screenshot skill.
