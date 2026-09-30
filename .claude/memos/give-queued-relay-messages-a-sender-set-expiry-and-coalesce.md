---
created: 2026-09-29 21:28:16
---

# Give queued relay messages a sender-set expiry and coalesce duplicates per project

Follows the store-and-forward queue memo. "Run /pull" stays valid for days; "commit my files" can be stale within hours, so the sender should set a per-message TTL. Several queued "run /pull" requests to one project should collapse into one delivery. Coalescing needs a key the sender supplies (a kind or a dedupe key) rather than body comparison.
