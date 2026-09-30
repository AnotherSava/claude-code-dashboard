---
created: 2026-09-29 21:28:17
---

# Choose the delivery trigger for queued relay messages: SessionStart context or registry appearance

Follows the store-and-forward queue memo. Two candidates:
- Inject on the recipient's SessionStart hook via additional_context, the same channel the instruction canary uses (http_server returns EventResponse.additional_context).
- Write to the inbox once session_registry shows the session live (messagingSocketPath appears).
SessionStart is earlier and needs no polling, but it lands as context rather than as a turn-starting message; the inbox write starts a turn, like a live relay, but needs a watch or a tick on the registry.
