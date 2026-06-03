---
name: dashboard-test-port-override
description: How to test SetupPanel (or any code that depends on an empty prompt_history) without breaking the production hook
metadata: 
  node_type: memory
  type: project
---

To make SetupPanel render reliably for inspection, drop:

    config/local.json  →  { "server_port": 9078 }

then `! deploy`. The deploy script copies local.json into `CONFIG_DEST` (the
runtime `config.json`), so the dashboard starts on 9078 while the hook in
`~/.claude/settings.json` still POSTs to 9077 — events fail (connection
refused), `prompt_history.json` stays empty, `has_history` stays false, and
SetupPanel keeps rendering across hide/show cycles.

To revert: delete `config/local.json` and redeploy.

**Why:** any active Claude Code conversation (including the one querying
about SetupPanel) fires hook events that get persisted to
`prompt_history.json`, which flips `has_history: true` and hides the panel
permanently. The port mismatch is the only way to inspect the panel
without disabling the hook globally.

**Also useful for:** anything else gated on `has_history` or
`sessions.length > 0` (empty-state UI, first-launch flows).
