---
name: sync-pusher-silence-anomaly
description: Unexplained ~5-min total pusher log-silence seen once on 2026-06-06; watch the `sync push cycle` trace for recurrence
metadata:
  type: project
---

On 2026-06-06 a running instance went completely log-silent — including the sync pusher — for ~5 minutes, then resumed; not reproduced or root-caused. A `sync push cycle` TRACE breadcrumb was added to `sync.rs::push_all` to detect recurrence.

**How to apply:** If sync mysteriously stops, grep `widget.jsonl` for gaps in `"sync push cycle"`. Still firing = pusher loop alive (look elsewhere); also silent = something stalled the loop / tokio runtime. See [[sync_device_pair]] for the device-pair setup.
