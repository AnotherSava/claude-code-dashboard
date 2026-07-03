---
name: user-prefers-generous-timing
description: Prefers generous timing on notification / reaction-time delays; default to the longer end
metadata:
  type: feedback
---

After the reading-time-scaled Telegram notify feature shipped, the user asked to "be more generous with time." The `notifications.telegram.reading_speed_cps` default was set to `10` (≈100 wpm) with a 6-min cap (`READING_CAP_MS`) — see the `notifications.rs` note in CLAUDE.md.

**Why:** A ping that fires before the user has finished reading a long reply is the annoyance they flagged. Erring slow (more delay) is preferred over snappy; the cost of a slightly-late ping is far lower to them than an early one.

**How to apply:** For any future timed/auto behavior in this project (notification windows, auto-dismiss, grace periods, reveal/animation timings), pick the generous end of a reasonable range by default and expose a knob rather than hard-coding a tight value. When choosing between two delays, prefer the longer.
