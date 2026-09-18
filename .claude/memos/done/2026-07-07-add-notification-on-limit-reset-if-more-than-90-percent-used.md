---
created: 2026-07-04 02:13:00
---

# add notification on limit reset if more than 90 percent used [DONE:…

add notification on limit reset if more than 90 percent used [DONE: `TelegramConfig.limit_reset_percent` (default 90, on). `ResetTracker` in notifications.rs detects a 5h/7d reset by the `resets_at` forward jump (>10min), NOT a pct drop — real data showed a 5h reset transiently zeroes BOTH buckets' pct for ~40min, so a 7d pct can crater without its window resetting; `peak_pct` running-max survives the dips. One-shot fire-and-forget ("5h limit reset (was 96%)"), buffered+retried on send failure, re-seeds on creds change. Backend-only (no frontend). Validated: 29-day history replay = 16 fires / 0 false positives; adversarial review caught 4 issues (peak over-report on direct jump, send-retry, creds-toggle re-seed, doc), all fixed.]
