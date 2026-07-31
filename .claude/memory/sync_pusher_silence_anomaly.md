---
name: sync-pusher-silence-anomaly
description: Unexplained ~5-min total pusher log-silence seen once on 2026-06-06; watch the `sync push cycle` trace for recurrence
metadata:
  type: project
---

On 2026-06-06 a running instance went completely log-silent — including the sync pusher — for ~5 minutes, then resumed; not reproduced or root-caused. A `sync push cycle` TRACE breadcrumb was added to `sync.rs::push_all` to detect recurrence.

**That breadcrumb is unreachable at the default log level.** It is `tracing::trace!`, while `logging.rs` defaults the filter to `info,claude_code_dashboard_lib=debug` — so it never reaches `widget.jsonl` and grepping for it always finds nothing (verified 2026-07-31: 0 occurrences in a 5.5 MB log). Raise the level first, or the check below is the practical substitute.

**How to apply:** A *successful* push logs nothing at all (`push_all` matches `Ok(resp) if resp.status().is_success() => {}`), so silence on `sync push failed` is the **success** signal — and is indistinguishable from a stalled loop. To positively confirm the pusher is alive without touching the log level, add a throwaway receive-only HTTP listener on localhost as a second `peers` entry and watch it receive the real push body; it advertises nothing back, so no remote row appears in the UI. Also check `sync push failed` entries for a *stale peer URL* before assuming a stall — after a peer rename, the failures name the old address. See [[sync_device_pair]] for the device-pair setup and [[debug_sync_fake_peer]] for the fuller synthetic-peer harness.
