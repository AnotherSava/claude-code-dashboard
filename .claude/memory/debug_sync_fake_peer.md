---
name: debug-sync-fake-peer
description: Test multi-device sync e2e with a Python fake peer instead of a second app instance
metadata: 
  node_type: memory
  type: project
---

Two app instances share the same app-data dir (config.json, ports), so don't test sync that way. Use a Python fake peer that **both pushes and serves** — under the pull model the receiver fetches content, so a client-only stub proves nothing.

1. POST a `SyncPush` to `127.0.0.1:9078/api/sync`: full session metadata with `dialog: []`, plus `dialog_tip` per session and a top-level `usage_tip`. No dialog or usage content rides the push.
2. Serve an HTTP server on the port you declared as `listen_port` — the dashboard derives the pull address from the connection's source IP plus that field, so it must be a port you really listen on. Answer `GET /api/sync/dialog?id=&since=` and `GET /api/sync/usage?since=`, both requiring `Authorization: Bearer <token>`.

No config edit needed: read the token straight from the app-data `config.json` and POST to the live listener. Expect `204`, then a pull within a second or two. Assert on the `since` values your server receives — `0` when the dashboard holds nothing for that session, its newest held timestamp on later rounds (that's the whole model: the receiver states its own position).

Verify landing via `remote_history/<DEVICE>.json` and `remote_usage/<DEVICE>.json`, plus the `dialog range pulled` / `usage range pulled` lines in `widget.jsonl`. A tip equal to what's held must produce **no** request. Clean up by deleting both files and restarting, or the fake device lingers as a row.

Verify UI rows via `PrintWindow` with `PW_RENDERFULLCONTENT` (flag 3) on the process `MainWindowHandle` — works when the widget is occluded (e.g. fullscreen video), unlike `CopyFromScreen`. Related: [[debug-synthetic-hook-events]].
