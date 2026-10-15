---
created: 2026-10-15 12:11:44
---

# Have the limit-reset Telegram ping report mid-window weekly resets, and stop carrying a spent window's peak across one

notifications.rs ResetTracker fires on an arrival-gated null-drop (prompt path) or a resets_at forward jump (fallback). A mid-window 7d reset keeps its seven_day_resets_at, so it passes neither: the ping never reports it. Its running peak_pct also spans the reset, so the next scheduled reset's 'was N%' can be the spent window's peak. June-October 2026 history holds ten such resets (06-09, 06-12, 06-18, 07-01, 07-09, 07-15, 08-27, 09-01, 09-04, 10-02). usage_history::seven_day_resets already detects them for the Work intensity chart (reading <=5% at least 10 points under the previous); next step: decide whether the ping should reuse that rule (single source) and reset peak_pct on it. Both limits are documented in the WindowReset doc comment and .claude/memory/usage_endpoint_weekly_resets.md.
