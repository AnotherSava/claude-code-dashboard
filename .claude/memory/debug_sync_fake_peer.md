---
name: debug-sync-fake-peer
description: Test multi-device sync e2e with a Python fake peer instead of a second app instance
metadata: 
  node_type: memory
  type: project
---

Two app instances share the same %APPDATA% dir (config.json, ports), so don't test sync that way. Instead: temporarily add a `sync` block to `config/local.json` (listen 9078, peers → `http://127.0.0.1:9080`, a test token), deploy, then run a Python script that (a) POSTs pushes to `:9078/api/sync` (auth/ingest/replay/own-name checks), (b) listens on `:9080` to capture the dashboard's outgoing pushes and verify watermark deltas (first push full dialog, later ones delta-only), (c) GETs `/api/sync/dialog?id=<local chat_id>&since=`. Revert `config/local.json` and redeploy after. Verify the UI rows via `PrintWindow` with `PW_RENDERFULLCONTENT` (flag 3) on the process `MainWindowHandle` — works when the widget is occluded (e.g. fullscreen video), unlike `CopyFromScreen`. Related: [[debug-synthetic-hook-events]].
