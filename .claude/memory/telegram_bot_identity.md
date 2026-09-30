---
name: telegram_bot_identity
description: Telegram pings go through the dashboard's dedicated notifications bot, token in Doppler claude-code-dashboard/dev; both machines are now configured
metadata:
  type: project
---

The dashboard's Telegram notifications are sent by the user's dedicated **notifications bot**, switched on 2026-09-28 from an older bot originally made for a trading project, after that bot's token was printed into a session transcript. The old token was revoked and verified refused with HTTP 401. The chat id is the user's private chat, unchanged by the switch. Ask the user for either bot's username rather than recording it here: this repo is public.

- **Windows (CHROME):** the token lives in Doppler `claude-code-dashboard` / `dev` as `TELEGRAM_BOT_TOKEN` (with `TELEGRAM_CHAT_ID`), rendered into the app config at deploy — see [[doppler_secret_storage]]. `getMe` on that token names the bot.
- **Mac (AIR):** configured 2026-09-29 on the user's instruction ("configure on par with chrome"), from the same two Doppler secrets written into `config/local.json`'s `notifications.telegram` and deployed. Nothing had to be asked of the Windows box: Doppler is the shared source, so parity is a read rather than a transfer. Verified at the layer that matters — the **installed** `config.json`'s token answered `getMe` as the expected bot and `getChat` reached the private chat, then a real `notification fired` line (`channel: telegram, status: done, reason: afk`) appeared within minutes. No test message was sent; `getChat` proves reachability without buzzing the user's phone.
- **The two machines take those secrets by different routes**, and only one of them is self-healing: CHROME renders `config/local.template.json` via Doppler at deploy, while AIR holds the plaintext values directly in gitignored `config/local.json` — see [[project_config_wiped_on_deploy]] and [[doppler_secret_storage]]. So a `git clean -xdf` or a fresh clone on the Mac silently loses Telegram and the sync token, and re-fetching them from Doppler is the repair.
- **Two config differences between the machines are deliberate and unresolved:** CHROME's template sets `instruction_canary_enabled: true` (default off), and its live config reads `high_alert: true` from a runtime tray toggle its own next deploy will reset. Neither was copied to AIR. Its `notifications.telegram` state windows are byte-identical to the compiled defaults, so nothing else diverges.
- Stale copies of the revoked token remain in dormant projects on the Windows box (a flight monitor's `.env`, an archived trading project, an unused `.env` in a 3D-models repo) and in scratch backups under this repo's `tmp/`. They are dead, not leaks.

**Why:** "which bot sends the pings and where is its token" is not recorded anywhere in the repo, and a rotation has to find every copy first.

**How to apply:** to rotate or change the bot, update the Doppler secret (pipe it from the clipboard, never print it), redeploy on Windows, and verify with `getMe` that the installed config answers as the expected bot. Never print `config.json`'s `notifications` block — it holds the token in plaintext.
