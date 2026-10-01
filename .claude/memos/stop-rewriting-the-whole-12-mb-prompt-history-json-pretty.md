---
created: 2026-10-01 03:59:15
---

# Stop rewriting the whole 12 MB prompt_history.json, pretty-printed, on every row removal

PromptHistoryStore::save_to_disk serializes the entire per-session map with indent 2 on every save; on 2026-10-01 the file was 12,328,886 bytes and growing without bound. commands::remove_session calls it between take_session and the per-row store cleanup, and since the RowLocks change (the /clear pid-forget race fix) that write runs while the row's lock is held, so every hook event for that row waits behind it. The first adversarial review of that fix noted a hold past the hook's 2s client timeout is possible (antivirus scanning the file, slow disk); the handler now runs on the blocking pool so the event is not lost, but it is late. Next step: measure save_to_disk duration in widget.jsonl, then either write compact JSON or persist per session (one file per chat_id, or an append log) so a removal writes only its own row.
