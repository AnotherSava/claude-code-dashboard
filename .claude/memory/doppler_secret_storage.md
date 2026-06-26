---
name: doppler_secret_storage
description: App secrets (Telegram bot_token/chat_id, sync token) live in Doppler, rendered into deploy config; how the render-then-wipe deploy works
metadata:
  type: project
---

The app's three secrets live in **Doppler project `claude-code-dashboard`, config `dev`**: `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `SYNC_TOKEN`. They are NOT stored in plaintext anywhere in the repo working tree.

**How they reach the app:** `config/local.template.json` (gitignored, per-machine, secret-free) holds the deploy config with secrets as `{{tojson .NAME}}` placeholders plus this device's non-secret prefs (sync peers, tray badge, notification windows). `scripts/deploy.sh` (the per-machine wrapper) runs `doppler secrets substitute config/local.template.json --project claude-code-dashboard --config dev > config/local.json`, then delegates to the global Tauri deploy (which copies `config/local.json` → app-data `config.json`), then a `trap ... EXIT` **wipes `config/local.json`** so the rendered secret-bearing file only exists transiently during a deploy.

To change a secret: `doppler secrets set NAME --project claude-code-dashboard --config dev` (use stdin to keep values out of shell history). To edit per-machine prefs (peers, badge): edit `config/local.template.json`. Both `config/local.json` and `config/local.template.json` are gitignored.

The running app reads `config.json` from its app-data dir, never `config/local.json` — so the wipe never affects a running instance (dev or deployed). The Anthropic OAuth tokens are NOT app secrets — the usage poller reads Claude Code's own credential store (`~/.claude/.credentials.json` / Keychain), so Doppler doesn't cover them.

Supersedes the older "token in config/local.json" detail in [[sync_device_pair]]: that token is now a Doppler placeholder. See also [[project_config_wiped_on_deploy]] (config.json is overwritten on deploy) and the global `feedback_doppler_secrets` / `tools-doppler-credentials` memories.
