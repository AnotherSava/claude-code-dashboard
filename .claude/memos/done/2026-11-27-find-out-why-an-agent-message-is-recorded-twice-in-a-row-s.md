---
created: 2026-11-08 08:38:35
---

# Find out why an agent message is recorded twice in a row's dialog

prompt_history.json holds every arriving agent message (local <cross-session-message> envelopes and relayed '[cross-machine message, relayed by the dashboard]' envelopes) as two identical user entries 9-41ms apart. Measured 2026-11-08 on Windows: 9 such pairs across claude, tauri-dashboard, what-is-next, transcripts and trips, and no person-typed prompt doubled under 1s (the closest genuine repeat was 2.3s apart). The history window now hides the second copy (HistoryApp.svelte deduplicatedDialog, identical text within DUPLICATE_WINDOW_MS = 1000), but the store still carries both, and anything else reading the dialog (sync pulls, notifications' read_burden_text, the investigate skill) sees two. Next step: find which two writers append the entry — likely the hook path (apply_set on UserPromptSubmit) and log_watcher's transcript read, or UserPromptExpansion plus UserPromptSubmit — by grepping widget.jsonl around one pair's timestamps, then dedupe at the writer and drop the frontend filter.
