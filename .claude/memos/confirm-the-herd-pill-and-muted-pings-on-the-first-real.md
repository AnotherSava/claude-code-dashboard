---
created: 2026-11-27 14:21:02
platform: windows
---

# Confirm the HERD pill and muted pings on the first real shepherd hold

The session_control feature (POST /api/session/control, deployed 2026-11-27) has been checked only through the route: mark answered controlled, the tmux pane title showed the sheep glyph, release answered released. Not yet observed in the running app: the widget row's HERD pill, that a state change of a marked row sends no per-state Telegram ping (and withdraws an outstanding one), and that pings resume after a release. The shepherd skills now mark sessions on every /fold and /herd (shepherd repo, control.py). Next step: during the first real run, watch a marked row in the widget, then grep widget.jsonl for decision=session_control lines on that row and check no 'notification fired' debug line appears for it while marked; after the release, a row left blocked should ping as usual.
