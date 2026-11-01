---
name: usage_endpoint_no_five_hour_bucket
description: "Since 2026-10-31 the OAuth usage endpoint returns five_hour: null (bucket absent) on a plan with no 5h limit, never as a reset transient; the transient keeps the bucket at 0 with a null resets_at"
metadata:
  type: project
---

On a plan with no 5-hour limit, `GET /api/oauth/usage` answers `five_hour: null` with `seven_day` still present. CHROME's records show it from 2026-10-31 15:10 MST, the Mac's from 2026-11-01 03:31 MST (it slept through the change), and every poll since. Across the ~26,000 records both machines held from June to that day, no bucket was ever absent, while the reset transient kept the bucket at `utilization: 0` with `resets_at: null` 850 times.

So an absent bucket means "this plan has no such limit", and a 0%/null bucket means "a reset is in flight". The dashboard relies on that split in two places: `UsageLimits::five_hour_limit` (one bar, no 5h tray badge) and `ResetTracker::observe`, which clears an absent bucket's tracker instead of reading it as a reset. If the endpoint is ever seen omitting a bucket mid-transient, both need revisiting together.

Before diagnosing an empty or missing 5h bar, read `usage_history.jsonl` for `five_hour_pct: null`.

Related: [[usage_endpoint_weekly_resets]].
