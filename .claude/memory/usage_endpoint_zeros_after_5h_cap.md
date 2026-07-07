---
name: usage_endpoint_zeros_after_5h_cap
description: "After the 5h limit hits 100%, the OAuth usage endpoint can transiently return utilization:0/resets_at:null for BOTH buckets — empty/IDLE bars then are an upstream artifact, not a bug"
metadata: 
  node_type: memory
  type: project
---

When the 5-hour limit reaches 100%, Anthropic's `GET /api/oauth/usage` endpoint can transiently return `utilization: 0.0, resets_at: null` for **both** the `five_hour` and `seven_day` buckets, then recover to real values a while later.

Observed 2026-07-01: `five_hour_pct` climbed to 100% (~13:36 local), both buckets flipped to `0.0` / `null` at ~14:16 and stayed there ~36 min (across a redeploy), then a direct query returned `five_hour: 2%` (real reset ~07-02 02:50) / `seven_day: 0%` (real reset ~07-06 03:00). `LimitBar` faithfully renders `utilization:0` + `resets_at:null` under status `ok` as `0% … IDLE` with an empty track — so **empty/IDLE bars appearing right after a limit maxes out are an upstream data artifact, not a dashboard bug; don't re-investigate them.** They self-correct on the next successful poll (≤10 min, the default interval), or immediately if the widget is re-shown (mount calls `refreshUsageLimits`). Note the stale-refresh effect in `LimitBar.svelte` bails on `resets_at === null`, so it won't proactively re-poll while the zeros are held.

Unexplained caveat: the 7d bucket dropped 51%→0% within the **same** weekly window (`seven_day_resets_at` unchanged at ~07-06 03:00), which is not a normal window reset — so this isn't just "the 5h window rolled over." Treat the zeros as an endpoint quirk, not real usage. The intensity chart's pct-drop reset detector will read the 100→0 as a reset.

This artifact is exactly why the **limit-reset Telegram ping** (`notifications.rs` `ResetTracker`, added 2026-07-04) detects a window reset by the **`resets_at` forward jump** (> a 10-min margin), *not* a pct drop: the 7d's pct cratering to 0 while its `resets_at` holds must NOT read as a 7d reset. Its `peak_pct` is a running `max()` so the transient dips can't lower the reported "was N%". Do not "unify" it with `build_week_chart`'s pct-drop detector — the two intentionally use opposite signals for opposite reasons (build_week_chart wants a pct drop clamped to 0; the ping wants the jump to reject the artifact).

Related: [[usage_five_hour_resets_at_jitter]].
