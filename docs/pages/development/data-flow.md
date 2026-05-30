---
layout: default
title: Data flow
parent: Development
nav_order: 3
---

End-to-end: what happens when a Claude Code hook fires, when a transcript file gets a new line, or when you toggle a tray menu item.

## The three input sources

```
┌──────────────────┐    POST /api/event     ┌────────────────┐
│  Claude Code     │──────────────────────▶ │  axum (Rust)   │
│  (hook forwards  │                        │  :9077         │
│   raw payload)   │                        └───────┬────────┘
└──────────────────┘                                │ adapters::dispatch
                                                    │ → apply_set
                                                    │   / apply_clear
                                                    ▼
┌──────────────────┐   notify::Event       ┌────────────────┐
│  transcript      │─────────────────────▶ │  AppState      │     app.emit
│  <session>.jsonl │                       │  Mutex<Vec<    │───────────────▶ Svelte
└──────────────────┘                       │    AgentSession│   "sessions_      (listen)
                                           │  >>            │    updated")
                                           └────────┬───────┘
┌──────────────────┐  #[tauri::command]             │
│  Svelte UI       │────────────────────▶ commands.rs ──apply_clear──▶ AppState
└──────────────────┘                                │
                                                    ▼
                                            Window / TrayIcon
                                            native APIs

┌──────────────────┐  file change event
│  config.json     │─────────────────▶ config_watcher reloads ─▶ emit("config_updated")
└──────────────────┘                                                       │
                                                                           ▼
                                                                  Svelte + tray refresh
```

Every mutation to session state funnels through `state::apply_set` or `state::apply_clear` so the sticky-label rules, working-time accumulator, and upgrade-only merge policy are enforced in one place regardless of origin.

## Path 1 — Hook POSTs event

1. Claude Code fires a lifecycle event (`UserPromptSubmit`, `Stop`, etc.). The hook command spawns `python claude_hook.py` and pipes the event payload to stdin.
2. `claude_hook.py` reads the payload, extracts `hook_event_name`, and POSTs `{client: "claude", event: <name>, payload: <verbatim>}` to `$TAURI_DASHBOARD_URL/api/event` (default `http://127.0.0.1:9077/api/event`). The hook does no classification or config reading.
3. `POST /api/event` hits the axum handler. Origin guard rejects non-null cross-origin requests.
4. `adapters::dispatch` routes by `client`; `adapters::claude::dispatch` matches on `event` and produces an `AdapterOutput::Set { input, transcript_path } | Clear { id } | Ignore`. All chat-id derivation, prompt cleaning, and transcript question-detection happen here.
5. For `Set`, `label_policy::select` decides the `(label, original_prompt)` pair.
6. Session-boundary marking. Claude `/clear` fires `SessionEnd` → `SessionStart`; the chat_id (derived from cwd) is unchanged but the JSONL is a new file. The handler covers this in two places: (a) on `Clear`, `AppState::mark_session_boundary` appends a `Separator` to the in-memory dialog and `PromptHistoryStore` persists it before `apply_clear` destroys the session — the following `SessionStart` then takes the "new" branch in `apply_set` and restores a dialog that already ends with the separator, so the upcoming user entry lands after it. (b) On `Set`, a defensive `transcript_path`-rotation check still calls `mark_session_boundary` if the new path differs from what `WatcherRegistry` is already watching — covers any rotation that happens without a preceding `SessionEnd`.
7. `AppState::apply_set` runs: if status transitions out of `working`, it accumulates elapsed time into `working_accumulated_ms`; if the transition is a task boundary (`done` / `idle` → `working`), it zeroes the accumulator; otherwise existing timers are preserved.
8. If `transcript_path` is present, `WatcherRegistry::start` spawns a per-session tokio task with a `notify::RecommendedWatcher` on the transcript's parent directory.
9. `emit_sessions_updated` broadcasts the fresh snapshot on the `sessions_updated` event.
10. The Svelte frontend's `listen` callback replaces its `$state` sessions array, Svelte's reactivity re-renders the list, the row updates within a frame.

## Path 2 — Transcript-driven updates

1. The watcher task from Path 1 is listening to filesystem events on the transcript's parent directory.
2. Claude Code writes a new JSONL line to the transcript. `notify` fires a `Modify` event; the watcher filters to events matching the exact transcript path.
3. The task sends a drain signal over an mpsc channel to itself. A 150ms debouncer collapses bursts (editors / streaming writes often produce several events per logical change).
4. `drain` reads the new bytes from the tracked byte offset, joins with leftover content from the previous drain, and splits into complete JSONL lines + a new leftover for the next call.
5. `infer_state` walks the new lines newest-first, skipping non-conversational entries (metadata, sidechains, synthetic errors). Returns the current `state`, latest `model`, latest summed input-side token count, and the latest assistant text block.
6. `apply_watcher_update` merges the metric inference into the session: watcher can set status to `working`, update `model`, update `input_tokens`, but cannot roll a session back to `done`, `idle`, `awaiting`, or `error` — hook events stay authoritative for terminal states. This avoids the race where the watcher reads a trailing assistant text as "done" while a fresh turn is already in flight. If the chunk produced a `latest_assistant_text`, `AppState::upsert_assistant_text` replaces the latest Assistant entry within the current turn (appends if none exists yet) and `PromptHistoryStore` persists the change. The watcher owns dialog text because Claude Code's `Stop` hook fires before the final assistant turn is flushed to JSONL — reading from the hook records the prior turn's text.
7. If anything changed, the session's `updated` timestamp refreshes and `emit_sessions_updated` fires exactly as in Path 1.

The initial drain on watcher startup suppresses the inferred **state** AND the **latest assistant text** — a resume would otherwise snap to a stale "done" from the prior turn and duplicate the last assistant entry already in the restored dialog. Model and token counts still surface.

Tauri commands have two possible targets: native window/tray APIs (`hide_window`, `show_window`, `toggle_window`, `quit_app`) or `AppState` itself — `remove_session` calls `apply_clear` to dismiss a row the user no longer cares about, then re-emits the snapshot on the same `sessions_updated` channel.

## Path 3 — Tray toggles

1. User clicks "Always on top" in the tray menu. `muda` fires a `MenuEvent` with the item's id.
2. The tray handler calls `window.set_always_on_top(new_state)` directly on the native window — no IPC round-trip.
3. `ConfigState::with_mut` flips `always_on_top` in the managed config. `ConfigState::save_to_disk` writes `config.json`.
4. The tray's `CheckMenuItem::set_checked` syncs the visual checkmark.
5. `emit_config_updated` broadcasts the new config. The frontend picks up the updated color thresholds, token-window lookup, and (future-proof) any UI-driving fields.

## Path 4 — External config edits

1. User edits `config.json` directly (via the "Open config/logs location" tray shortcut or any editor).
2. `config_watcher` — a `notify::RecommendedWatcher` on the config directory — receives a `Modify` event.
3. The 150ms debouncer waits for any rename-based atomic writes to settle.
4. `Config::load_or_default` re-reads the file. Serde serializes both the new and current in-memory configs to JSON strings; if they're byte-identical, the reload is skipped — this is how our own tray writes avoid re-triggering the reload path.
5. `apply_config_to_window` applies runtime-safe changes (always-on-top, saved window position). Port changes are intentionally ignored on hot-reload and require a restart.
6. `config_updated` is emitted and the tray check marks re-sync.

## Sticky-label state machine

The `(label, original_prompt)` decision rules and the UI display rule live in [Sticky labels](sticky-labels). Every `apply_set` call funnels through `src-tauri/src/label_policy.rs::select`, so the rules are enforced in one place regardless of which path fired the event.
