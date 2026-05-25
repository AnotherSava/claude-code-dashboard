# Persist refined dialog history per session

## Context

The dashboard tracks per-session prompt history (`previous_prompts`, `task_started_at`) but stores it only in memory — every app restart wipes the history. The user wants to (a) capture richer data — every user message AND every agent response as separate timestamped entries — and (b) persist it across restarts so the tooltip and future visualizations always have full context.

## Data model

A flat chronological list of entries, each with its own role and timestamp. NOT paired turns — user messages and agent messages are independent entries, allowing consecutive same-role entries (e.g. user cancels and retypes, or agent responds then follows up).

New structs in `state.rs`:

```rust
enum DialogRole { User, Assistant }

struct DialogEntry {
    role: DialogRole,
    text: String,
    timestamp: i64,
    /// The session status AFTER this event was processed.
    /// User entries: always Working.
    /// Assistant entries (Stop): Done or Awaiting.
    status: Status,
}
```

No `is_task_prompt` field — it's derived from the sequence. A User entry is a task prompt when the **previous** entry's `status` was `Done`, `Idle`, `Working`, or the entry is the first in the dialog. `Awaiting` → `Working` is an approval cycle, not a task boundary. Continuation-prompt filtering uses `Config::continuation_prompts` at display time (already available to the frontend via config state).

Added to `AgentSession` as `dialog: Vec<DialogEntry>` with `#[serde(default)]`. The existing `previous_prompts` field and `PromptHistoryEntry` struct are **removed** — the tooltip derives task prompts from the dialog sequence (see derivation rule above). `original_prompt` and `task_started_at` stay (used by `displayLabel` and the state machine).

## When entries are created

| Event | Role | Text source | status |
|-------|------|-------------|--------|
| `UserPromptSubmit` | User | `input.label` (cleaned prompt) | `Working` |
| `Stop` | Assistant | `last_assistant_text(transcript)`, stored in full | `Done` or `Awaiting` (from classify) |

Other events (`PreToolUse`, `Notification`, `SessionStart`, `SessionEnd`) do NOT create dialog entries. `Notification(idle_prompt)` still uses `last_assistant_text` for question detection but does NOT push an Assistant entry (avoids duplicating the Stop entry for the same response).

## Agent text: storage vs display

Assistant text is **stored in full** — no truncation at storage time. The persistence file may grow larger for long-running sessions, but disk space is cheap and different visualizations may want different levels of detail.

A **display-time** function `cleanAgentText(text, limit)` in the frontend (`src/lib/types.ts`) produces a compact summary for tooltips and compact views. Priority cascade:

1. **Whole text** — if it fits within `limit` chars, return as-is
2. **Last paragraph + first paragraph + truncated middle** — last paragraph has highest priority (usually the question/summary), first paragraph second (context), remaining middle truncated with `…`
3. **Last paragraph + first paragraph** — if middle truncation still exceeds limit, drop the middle entirely, join with `\n\n…\n\n`
4. **Last paragraph only** — if first + last don't fit
5. **Truncated last paragraph** — if even the last paragraph exceeds `limit`, truncate with `…`

Default limit: 2000 chars.

## Threading entries through SetInput

`SetInput` gets one new field:

```rust
pub struct SetInput {
    // ... existing fields ...
    pub dialog_entry: Option<PendingDialogEntry>,
}

pub struct PendingDialogEntry {
    pub role: DialogRole,
    pub text: String,
}
```

The adapter builds the entry in `dispatch()` — for `UserPromptSubmit` from the label, for `Stop` from `last_assistant_text`. The state layer (`apply_set`) adds `timestamp: now_ms` and sets `status` from `input.status` (the status the session is transitioning into).

## apply_set changes

Signature adds one parameter (restored dialog for persistence) and returns `bool` (true when dialog was modified):

```rust
pub fn apply_set(
    &self,
    input: SetInput,
    now_ms: i64,
    continuation_prompts: &[String],
    restored_dialog: Option<Vec<DialogEntry>>,
) -> bool
```

At the end of both the existing-session and new-session paths, if `input.dialog_entry` is `Some`, convert to a full `DialogEntry` and push to `session.dialog`. Tests pass `None` for the restored parameter.

## Persistence store

New module `prompt_history.rs` with `PromptHistoryStore`, mirroring the `ConfigState` pattern:

- `Mutex<HashMap<String, PersistedSession>>` + `PathBuf`
- `PersistedSession`: `dialog`, `original_prompt`, `task_started_at`
- File: `app_data/prompt_history.json`
- Methods: `new(path)` (loads from disk), `get(id)`, `save_session(id, session_fields)`, `remove(id)`, `save_to_disk()`
- Managed as Tauri state in `lib.rs`

## Write strategy

Save after every `apply_set` that returns `true` (dialog changed). The caller in `http_server.rs`:

1. Pre-fetch restored dialog from store for the session id
2. Call `apply_set`, capture the bool
3. If true: read the session's current persisted fields from AppState, call `store.save_session(id, ...)` + `store.save_to_disk()`

Also save on `remove_session` (call `store.remove(id)`).

NOT called from the log watcher path (it uses `apply_watcher_update`, doesn't touch dialog).

## Restoration on new session creation

When `apply_set` creates a new session and `restored_dialog` is `Some(entries)`:

1. Set `dialog = entries.dialog`
2. Set `original_prompt` from persisted value (unless the event itself sets one)
3. Set `task_started_at` from persisted value (unless event sets `original_prompt`)

This means the first event after restart (typically a Stop with status=Done) creates the session with all history restored. The next UserPromptSubmit then works normally — task boundary fires, captures the new prompt as `original_prompt`.

## Files to modify

- `src-tauri/src/state.rs` — add DialogEntry/DialogRole/PendingDialogEntry; remove PromptHistoryEntry + previous_prompts + its push logic; add dialog field; extend SetInput; change apply_set signature + return
- `src-tauri/src/prompt_history.rs` — new module (PromptHistoryStore + PersistedSession)
- `src-tauri/src/adapters/claude.rs` — build PendingDialogEntry in dispatch for UserPromptSubmit and Stop (full text, no truncation)
- `src-tauri/src/http_server.rs` — get store, pre-fetch dialog, pass to apply_set, persist on change
- `src-tauri/src/commands.rs` — remove_session cleans store
- `src-tauri/src/lib.rs` — add mod, manage store, update seed_dev_sessions
- `src/lib/types.ts` — replace PromptHistoryEntry with DialogEntry/DialogRole; remove previous_prompts; add dialog
- `src/lib/components/SessionItem.svelte` — derive tooltip task prompts from dialog sequence instead of previous_prompts
- `src/lib/mockSessions.ts` — replace previous_prompts with dialog fixtures
- Test fixtures in `label_policy.rs`, `log_watcher.rs`, `notifications.rs` — remove previous_prompts, add dialog

## Verification

1. `cargo test --lib` — all existing tests pass with new fields + return type
2. New tests: dialog entry creation (User on task boundary, User on approval, Assistant on Stop, new session with restore); persistence store load/save round-trip
3. `svelte-check` — frontend types clean
4. Deploy, submit 3+ prompts to the same project, verify `prompt_history.json` grows with dialog entries including both User and Assistant entries
5. Restart dashboard (re-deploy), verify tooltip shows prior prompts from persisted dialog
