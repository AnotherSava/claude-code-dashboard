---
created: 2026-09-10 07:52:00
---

# make sure that app restarts do not cause extra limit usage api calls (done 2026-09-11: a launch fired TWO requests

`request_refresh` raced the opening poll and `Notify` parked a permit redeemed the instant it returned — and then every launch fired one more. Both are closed: the refresh gate now measures against poll attempts rather than the reading's age, and `poll_once` skips its opening poll outright when `usage_cache.json` holds a reading younger than the interval, replayed as `Ok` because it is no staler than what a running instance would be showing. `usage_cache.rs` also persists the endpoint's own `Retry-After` so a deadline survives the restart that used to reset it. Observed live: `opening poll skipped; the stored reading is younger than the interval age_ms=60876`.)
