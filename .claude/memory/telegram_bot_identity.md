---
name: telegram_bot_identity
description: Telegram pings go through the dashboard's dedicated notifications bot, token in Doppler claude-code-dashboard/dev; only the Windows box (CHROME) has Telegram configured
metadata:
  type: project
---

The dashboard's Telegram notifications are sent by the user's dedicated **notifications bot**, switched on 2026-09-28 from an older bot originally made for a trading project, after that bot's token was printed into a session transcript. The old token was revoked and verified refused with HTTP 401. The chat id is the user's private chat, unchanged by the switch. Ask the user for either bot's username rather than recording it here: this repo is public.

- **Windows (CHROME):** the token lives in Doppler `claude-code-dashboard` / `dev` as `TELEGRAM_BOT_TOKEN` (with `TELEGRAM_CHAT_ID`), rendered into the app config at deploy — see [[doppler_secret_storage]]. `getMe` on that token names the bot.
- **Mac (AIR):** Telegram has never been configured — the installed `config.json` has an empty `bot_token` and `chat_id`, and `config/local.json` has no telegram block. Enabling it there is new setup, not a rotation, and is the user's call.
- Stale copies of the revoked token remain in dormant projects on the Windows box (a flight monitor's `.env`, an archived trading project, an unused `.env` in a 3D-models repo) and in scratch backups under this repo's `tmp/`. They are dead, not leaks.

**Why:** "which bot sends the pings and where is its token" is not recorded anywhere in the repo, and a rotation has to find every copy first.

**How to apply:** to rotate or change the bot, update the Doppler secret (pipe it from the clipboard, never print it), redeploy on Windows, and verify with `getMe` that the installed config answers as the expected bot. Never print `config.json`'s `notifications` block — it holds the token in plaintext.
