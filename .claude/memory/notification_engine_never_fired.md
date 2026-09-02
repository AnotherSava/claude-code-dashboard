---
name: notification-engine-never-fired
description: The per-state notification windows are compiled defaults that have never fired in production — call them untested, not tuned
metadata:
  type: project
---

As of 2026-09-01 the notification engine in `notifications.rs` has never delivered a single notification. Telegram `bot_token` and `chat_id` are both empty, `TelegramNotifier` is the only `Notifier`, and `widget.jsonl` from 2026-06-04 to 2026-09-01 contains zero "notification fired" lines.

**Why:** the AFK windows, reaction backstops, reading-time deferral and retry backoff read as a tuned system — the code is careful and heavily commented — but no value in it has ever been observed against real behavior. Describing it as "already tuned" (as an outside reader naturally does) turns an untested default into a settled decision and skips the measurement that would justify it.

**How to apply:**
- The live `blocked` rule is `afk 60s` / `reaction 120s`, but `fire_reason` adds `reading_ms` to each window — across the user's 501 real blocked turns that works out to a **~5.4 min median**, not the 2 min the config reads like. Compute the effective window before quoting a config number.
- When a delivery channel is finally wired, treat the first weeks as calibration and expect the windows to move.
- Grep `decision`-tagged lines and the `"notification fired"` debug line in `widget.jsonl` to check whether anything has actually fired before reasoning about behavior.

Related: [[notification-delivery-channel]], [[debug_state_transitions_via_widget_jsonl]], [[notification_text_mirrors_primary_text]].
