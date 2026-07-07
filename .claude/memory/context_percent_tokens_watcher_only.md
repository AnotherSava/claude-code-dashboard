---
name: context_percent_tokens_watcher_only
description: input_tokens (hence context_percent and every context-% feature) is populated only by the transcript watcher, so it's None post-restart and can't be faked with synthetic hook events
metadata:
  type: project
---

`input_tokens` — and therefore `context_percent` and every context-% feature (Telegram/tray context alerts, the terminal-title ` [N%]` suffix) — is populated **only** by the transcript watcher parsing usage lines from the JSONL, **never** by hook events (the hook path carries no tokens).

Verification/testing consequences:

- **Post-restart it's `None`.** Right after a deploy/app restart, `input_tokens` is `None` for every session until it next produces assistant output, so `context_percent` returns `None` and context-% features render nothing. You cannot observe them immediately after a deploy — wait for a real session to emit, or drive one.
- **Synthetic hook events can't drive them.** A fake-cwd session ([[debug_synthetic_hook_events]]) has no tokens, so it never shows a percentage. Useful only to force a global `emit_sessions_updated` → `terminal_title::sync` over the *other* (real) sessions.
- To see a context-% feature live, use a real session that has produced output and is above the threshold.

Related gotcha found the same session: config hot-reload (`config_watcher`) does **not** re-sync terminal titles — `terminal_title::sync` runs only inside `commands::emit_sessions_updated`, so a changed `terminal_title_context_percent` / `terminal_titles` takes effect on the next session activity, not the instant the config is saved (same trait as the `terminal_titles` tray toggle).
