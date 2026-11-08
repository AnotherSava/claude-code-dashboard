---
created: 2026-10-01 03:59:15
---

# Stop rewriting the whole 12 MB prompt_history.json, pretty-printed, on every row removal

PromptHistoryStore::save_to_disk serializes the entire per-session map with indent 2 on every save; on 2026-10-01 the file was 12,328,886 bytes and growing without bound. commands::remove_session calls it between take_session and the per-row store cleanup, and since the RowLocks change (the /clear pid-forget race fix) that write runs while the row's lock is held, so every hook event for that row waits behind it. The first adversarial review of that fix noted a hold past the hook's 2s client timeout is possible (antivirus scanning the file, slow disk); the handler now runs on the blocking pool so the event is not lost, but it is late. Next step: measure save_to_disk duration in widget.jsonl, then either write compact JSON or persist per session (one file per chat_id, or an append log) so a removal writes only its own row.

Measured 2026-11-08 (unattended backlog pass, Windows): the file is now 16,726,990 bytes. Compact JSON is 15,446,951, only 8% smaller, so dropping the indent does not fix this and was not done. The four largest rows (printlab, claude, tauri-dashboard, what-is-next) are 2.3–2.7 MB each and grow without bound. What remains is a layout or retention decision for the user: one file per chat_id (a removal writes only its row, but `BACKUP_FILES` in `config/deploy.env` and several debugging memories name the single file), or a cap on how much dialog a row keeps.
