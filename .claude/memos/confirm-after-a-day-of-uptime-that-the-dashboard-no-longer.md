---
created: 2026-11-25 12:01:43
platform: macos
---

# Confirm after a day of uptime that the dashboard no longer grows to tens of GB

The 2026-11-25 fix stopped every sessions_updated emit carrying each row's whole dialog (~10 MB) to the main and history webviews. Before it, the Rust process's Malloc Large grew ~25-50 MB per emit and the app reached 25 GB in 17 h; the 2026-11-13 reading also showed its two WebContent processes at 15 GB + 10 GB compressed. After deploy it held at 150-225 MB for 5 minutes, which cannot show day-scale growth in either. Next step: after 24 h or more of uptime, run footprint <pid> on claude-code-dashboard and on each com.apple.WebKit.WebContent process started within a second of it; if the app is still near a few hundred MB and the webviews small, the fix holds. If not, sample Malloc Large once a second and line its steps up against widget.jsonl as the 2026-11-25 investigation did (learnings/webview-push-payload-memory-growth.md).
