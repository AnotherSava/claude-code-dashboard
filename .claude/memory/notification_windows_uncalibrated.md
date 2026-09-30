---
name: notification-windows-uncalibrated
description: The per-state notification windows are compiled defaults that only started firing on 2026-09-29 — call them uncalibrated, not tuned, and compute the effective window rather than quoting the config
metadata:
  type: project
---

The notification engine in `notifications.rs` delivered **nothing at all** from 2026-06-04 to 2026-09-28: `widget.jsonl` held zero `notification fired` lines across that whole span, because `bot_token` and `chat_id` were empty and `TelegramNotifier` is the only `Notifier`. That ended on 2026-09-29, when Telegram was configured on the Mac ([[telegram_bot_identity]]) and the first real ping fired within minutes — `channel: telegram, id: claude, status: done, reason: afk`.

**Why:** the AFK windows, reaction backstops, reading-time deferral and retry backoff read as a tuned system — the code is careful and heavily commented — but no value in it had ever been observed against real behavior. Describing it as "already tuned" (as an outside reader naturally does) turns an untested default into a settled decision and skips the measurement that would justify it. Firing does not change that: the numbers are the same compiled defaults, now finally observable.

**How to apply:**
- Treat the period from 2026-09-29 as calibration and expect the windows to move. The first weeks of real pings are the only evidence that has ever existed about whether they are right.
- The live `blocked` rule is `afk 60s` / `reaction 120s`, but `fire_reason` adds `reading_ms` to each window — across the user's 501 real blocked turns that works out to a **~5.4 min median**, not the 2 min the config reads like. Compute the effective window before quoting a config number.
- Only the Mac fires under the default config; the Windows box additionally runs with `high_alert: true` set from its tray, which short-circuits both windows entirely. So the two machines are not sampling the same rules, and a ping's timing there says nothing about these defaults.
- Grep `decision`-tagged lines and the `"notification fired"` debug line in `widget.jsonl` to check what has actually fired before reasoning about behavior.

Related: [[notification-delivery-channel]], [[debug_state_transitions_via_widget_jsonl]], [[notification_text_mirrors_primary_text]], [[user_prefers_generous_timing]].
