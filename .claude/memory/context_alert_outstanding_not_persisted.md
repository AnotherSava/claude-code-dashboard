---
name: context_alert_outstanding_not_persisted
description: Telegram context-alert tracking is in-memory only; orphaned messages after an app restart are a known, deliberately-unfixed limitation
metadata:
  type: project
---

The `context_outstanding` map in `notifications.rs` (session id → Telegram message_id) is recreated empty on every app launch. A context-usage alert sent by one app instance can't be dismissed by a later instance — its message_id handle is lost — so an app restart *between* the alert firing and the session dropping-below / `/clear` orphans the Telegram message permanently (and a still-over session gets a fresh duplicate alert on the next launch).

Considered persisting the map to disk (the [[project_config_wiped_on_deploy]] pattern) but **deliberately declined**: it only helps across restarts, which are rare in normal always-on operation, and there's no way to delete a prior-instance message without persisting its id — so it's persist-or-nothing for marginal benefit. The per-state `outstanding` map has the same latent limitation.

If the "stuck context notification after `/clear`" symptom recurs, *this* is the cause — not a dismiss bug. The send/dismiss path is now logged in `widget.jsonl` with `decision="context_alert"` / `decision="context_dismiss"` (greppable), so the alert lifecycle is reconstructable.
