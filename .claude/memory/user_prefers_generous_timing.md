---
name: user-prefers-generous-timing
description: Prefers generous timing on notification / reaction-time delays; default to the longer end
metadata:
  type: feedback
---

After the reading-time-scaled Telegram notify feature shipped, the user asked to "be more generous with time." The `notifications.telegram.reading_speed_cps` default was set to `10` (≈100 wpm) with a 6-min cap (`READING_CAP_MS`) — see the `notifications.rs` note in CLAUDE.md.

**Why:** A ping that fires before the user has finished reading a long reply is the annoyance they flagged. Erring slow (more delay) is preferred over snappy; the cost of a slightly-late ping is far lower to them than an early one.

**How to apply:** For any future timed/auto behavior in this project (notification windows, auto-dismiss, grace periods, reveal/animation timings), pick the generous end of a reasonable range by default and expose a knob rather than hard-coding a tight value. When choosing between two delays, prefer the longer.

**A delay also earns its place when a notification duplicates something already on screen.** Then it is not a rate limit, it is the window in which the cheaper channel gets to win, so the ping only ever arrives for the times nobody was looking. Raised by the user on `stale_tab_alert_ms`: the row's `≠` badge appears the moment a stale terminal tab is caught, so the alert waits ten minutes before sending and the fix usually happens first. Reach for this shape whenever a condition surfaces in the widget *and* on the phone, and make the delay a config field rather than a constant, so turning it to `null` also turns the alert off.
