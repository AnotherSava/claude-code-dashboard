# Lock chat_id to session_id to stop mid-session row fragmentation

## Context

A single Claude Code conversation can fragment into multiple dashboard rows. Observed live: one BGA-assistant session split into an `assistant` row and a `data` row — the JSON prompt `"{"realTime":false,...}"` landed in `data` while the rest stayed in `assistant`, and the same assistant response appeared in both rows.

Root cause: `adapters::claude::derive_chat_id` (claude.rs:103) derives the row id purely from the hook payload's `cwd`, which changes mid-session when the agent `cd`s into a subdirectory. `session_id` is only a fallback when cwd is absent. Verified:
- This session (`36e5a787`) has one session_id but two cwds: `tauri-dashboard` (1534 entries) and `tauri-dashboard\src-tauri` (47). The src-tauri events would fragment to a `src-tauri` row.
- `/clear` mints a **new** session_id (new transcript file) but keeps the same cwd — so the current cwd-derivation is what keeps a row continuous across `/clear`.

So the fix must lock chat_id per session_id, **and** still derive from cwd for unseen session_ids so `/clear`'s new session_id re-derives the same name.

## Approach

Introduce a `ChatIdRegistry` (managed state, persisted) mapping `session_id → chat_id`. Resolve the stable id in `http_server` right after `dispatch`, overriding the cwd-derived id. The adapter stays a pure function — `derive_chat_id` is unchanged and remains the first-seen derivation.

### Resolution rule

`resolve(session_id, derived) -> chat_id`:
- `session_id` empty → return `derived` (can't lock; preserves the no-cwd/no-session fallback).
- session_id already mapped → return the stored chat_id (this absorbs mid-session cwd changes).
- otherwise → insert `session_id → derived`, persist, return `derived`.

Because `SessionStart` fires first with the project-root cwd, the first event anchors the lock to the root-derived name. `/clear` → new session_id, same cwd → first event re-derives the same name → continuity holds (the existing `mark_session_boundary` path in http_server still works, keyed on the resolved chat_id).

On `SessionEnd` (`Clear`): resolve to the right chat_id (to clear the correct row), then drop the session_id entry and persist (session is over; a following `/clear` SessionStart re-creates it from cwd).

### Changes

**New `src-tauri/src/chat_id_registry.rs`** — mirror `prompt_history.rs`:
- `ChatIdRegistry { path, data: Mutex<HashMap<String,String>> }`
- `new(path)` loads `session_chat_ids.json` from app data dir (tolerate missing/corrupt → empty, same as `PromptHistoryStore::new`)
- `resolve(&self, session_id: &str, derived: &str) -> String`
- `forget(&self, session_id: &str)` (called on SessionEnd)
- save-to-disk on mutation

**`src-tauri/src/lib.rs`** — register it: `app.manage(ChatIdRegistry::new(app_data.join("session_chat_ids.json")))` in `setup()`, alongside the existing `PromptHistoryStore` registration. Add `mod chat_id_registry;`.

**`src-tauri/src/http_server.rs`** — after `adapters::dispatch`, extract `session_id` from `req.payload` and rewrite the output id:
- `Set { input, .. }` → `input.id = registry.resolve(&session_id, &input.id)`
- `Clear { id }` → `let resolved = registry.resolve(&session_id, &id); registry.forget(&session_id); id = resolved`
- `Ignore` → unchanged

Reuse `app.try_state::<ChatIdRegistry>()` like the existing `PromptHistoryStore` access.

**Adapter** — no change to `derive_chat_id` or `dispatch`. (Optional: extend the dispatch debug log to note when the resolved id differs from the derived id, for future diagnosis.)

### Why persist

The deploy workflow restarts the dashboard frequently, and it tracks sessions across all projects. Without persistence, a restart while some session's cwd is in a subdirectory would re-fragment (re-derive the subdir name and create a new row). A small `session_chat_ids.json` loaded on startup avoids this, mirroring how `prompt_history.json` already survives restarts.

## Verification

1. `cargo test` — add unit tests for `ChatIdRegistry::resolve`:
   - unseen session_id → returns derived and stores it
   - seen session_id with a different derived (cwd changed) → returns the original stored id
   - empty session_id → returns derived, stores nothing
   - `forget` drops the mapping
2. Manual: deploy, start a session, have the agent `cd` into a subdirectory and run a command, then submit a prompt — confirm it stays one row (not a new subdir-named row).
3. Manual `/clear`: run `/clear` in a tracked session, submit a new prompt — confirm the row persists with the prior dialog restored and a separator (existing behavior intact).
4. Restart mid-session (deploy) while cwd is at root and again while in a subdir — confirm no new row is spawned in either case (persistence working).
