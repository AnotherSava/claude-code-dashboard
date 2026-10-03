---
created: 2026-10-02 22:42:41
---

# Stop the history window hiding a prompt whose first 10 characters match the next one

HistoryApp.svelte's deduplicatedDialog drops a user entry when the next entry is also a user entry and the two share their first 10 characters (entry.text.slice(0, 10)). It exists to collapse a prompt recorded twice back to back, but it also hides genuinely distinct prompts: 'pull remote changes and resolve conflicts' followed by 'pull remote changes and push' shows as one entry. Flagged by the Mac tauri-dashboard session on 2026-10-02 while fixing the sync merge, which now preserves real back-to-back repeats. Shape of a fix: compare the whole text (or the whole text plus a timestamp window), not a 10-character prefix; check against prompt_history.json how many stored pairs each rule would collapse before choosing.
