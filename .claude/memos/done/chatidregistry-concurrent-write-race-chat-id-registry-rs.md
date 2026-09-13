---
created: 2026-06-27 16:31:00
---

# ChatIdRegistry concurrent-write race (chat_id_registry.rs)

session_chat_ids.json is written with no atomic write/lock, so overlapping writes (e.g. two rapid hook events) can corrupt or lose the session_id->chat_id mapping. Low frequency but a real correctness gap. Fix = write-to-temp + atomic rename (and/or an in-process lock around the read-modify-write). [DONE: save_to_disk now writes a sibling .tmp then atomic-renames over the target; the mutex is held through the rename so overlapping saves stay totally ordered. The in-process lock already existed — the real gap was non-atomicity (a torn file reloads as empty and drops every mapping).]
