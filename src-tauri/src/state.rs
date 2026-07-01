use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Idle,
    Working,
    /// Held active by background work after the main turn already settled —
    /// "looks done but isn't". Set at `Stop` time from the hook's
    /// `background_tasks` payload (see `adapters::claude::classify_stop`); the
    /// next turn's `Stop` (empty `background_tasks`) settles it to `Done`.
    /// Rendered light-blue as "WAIT".
    Waiting,
    /// Blocked on the user: a question, a tool-permission prompt, or an MCP
    /// elicitation. Rendered amber as "BLOCK". (Formerly `Blocked`.)
    Blocked,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogRole {
    User,
    Assistant,
    Separator,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DialogEntry {
    pub role: DialogRole,
    pub text: String,
    pub timestamp: i64,
    pub status: Status,
    /// True when this entry is a user prompt that started a fresh task — the
    /// same boundary decision `apply_set` uses for the sticky label. The
    /// frontend reads this directly for the history highlight and the row
    /// tooltip, instead of re-deriving boundaries with a divergent heuristic.
    /// `#[serde(default)]` so dialogs persisted before this field existed load
    /// as `false` (those pre-flag entries simply aren't highlighted).
    #[serde(default)]
    pub task_start: bool,
}

/// Built by the adapter, converted to a full [`DialogEntry`] by `apply_set`
/// (which adds `timestamp` and `status`).
#[derive(Clone, Debug)]
pub struct PendingDialogEntry {
    pub role: DialogRole,
    pub text: String,
}

/// Fields persisted to `prompt_history.json` and restored on session
/// re-creation after an app restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedSession {
    #[serde(default)]
    pub dialog: Vec<DialogEntry>,
    #[serde(default)]
    pub original_prompt: Option<String>,
    #[serde(default)]
    pub task_started_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub status: Status,
    /// The status the row held immediately before its current `Working` turn
    /// began — captured by `apply_set` on every non-Working → Working
    /// transition. A turn cancelled with Esc (no `Stop` hook) reverts here
    /// rather than collapsing to `Idle`, so an aborted reply to a pending
    /// question leaves the row in the `Blocked` state the question put it in
    /// (the next real answer is then an approval-cycle reply, not a new task).
    /// Internal bookkeeping — never serialized to the frontend / sync / disk.
    #[serde(skip)]
    pub status_before_working: Status,
    pub label: String,
    pub original_prompt: Option<String>,
    #[serde(default)]
    pub task_started_at: i64,
    #[serde(default)]
    pub dialog: Vec<DialogEntry>,
    pub source: String,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub updated: i64,
    pub state_entered_at: i64,
    pub working_accumulated_ms: u64,
    /// User-assigned display name, resolved from `CustomNamesStore` at emit
    /// time (keyed by `id`). Always `None` in `AppState`; filled on the way
    /// to the frontend. Not persisted in `prompt_history`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Device name of the peer dashboard this session was synced from; `None`
    /// for sessions running on this machine. Stamped by `sync::ingest` (which
    /// also namespaces `id` to "{device}/{raw_id}"); the frontend renders the
    /// device badge from it. Always `None` in `AppState.sessions`.
    #[serde(default)]
    pub origin: Option<String>,
}

impl AgentSession {
    /// Name to show the user in notifications and titles: the custom display
    /// name if one is set, else the chat_id. `display_name` is only populated
    /// off the `CustomNamesStore` (see [`crate::custom_names::CustomNamesStore::apply`]),
    /// so this reads the chat_id anywhere that overlay hasn't been applied.
    pub fn display_label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug)]
pub struct SetInput {
    pub id: String,
    pub status: Status,
    pub label: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub dialog_entry: Option<PendingDialogEntry>,
}

/// True when `label` (after trim, case-insensitive) matches one of the
/// configured continuation phrases. Used by `apply_set` to suppress a
/// task boundary so a "go" / "continue" / "proceed" reply after a Done
/// status doesn't reset `original_prompt` and the working timer.
fn is_continuation_prompt(label: &str, continuation_prompts: &[String]) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return false;
    }
    continuation_prompts
        .iter()
        .any(|p| p.trim().eq_ignore_ascii_case(trimmed))
}

/// Sessions pushed by one peer dashboard. The in-memory set is repopulated
/// by the peer's next push after a restart; accumulated dialogs are backed
/// by `remote_history` on disk and re-seeded at ingest. Kept separate from
/// `AppState::sessions` so every local-session consumer (`apply_set`,
/// `prompt_history`, `notifications`, `terminal_title`, `log_watcher`) stays
/// remote-blind by construction instead of by per-call filtering.
#[derive(Clone, Debug)]
pub struct RemoteDevice {
    /// Already namespaced ("{device}/{raw_id}") and origin-stamped.
    pub sessions: Vec<AgentSession>,
    /// Receiver-clock ms of the last push from this device — TTL reaping.
    pub last_seen: i64,
    /// Base URL for catch-up dialog fetches, derived from the push's socket
    /// peer IP + advertised listen_port (e.g. "http://100.1.2.3:9078").
    pub origin_addr: String,
}

#[derive(Default)]
pub struct AppState {
    /// Sessions running on this machine — the only set the hook/watcher
    /// pipeline, persistence, and notifications ever touch.
    pub sessions: Mutex<Vec<AgentSession>>,
    /// Sessions synced from peer dashboards, keyed by device name. BTreeMap
    /// so the emit-time merge produces a stable row order across emits.
    pub remote: Mutex<BTreeMap<String, RemoteDevice>>,
}

/// Append a session-boundary separator to a session's dialog in place. Returns
/// whether one was added — skipped when the dialog is empty or already ends with
/// a separator. Shared by [`AppState::mark_session_boundary`] and
/// [`AppState::take_session`] so the boundary rule lives in exactly one place.
fn append_boundary(session: &mut AgentSession, now_ms: i64) -> bool {
    if session.dialog.is_empty() {
        return false;
    }
    if session.dialog.last().is_some_and(|e| e.role == DialogRole::Separator) {
        return false;
    }
    session.dialog.push(DialogEntry {
        role: DialogRole::Separator,
        text: String::new(),
        timestamp: now_ms,
        status: Status::Idle,
        task_start: false,
    });
    session.updated = now_ms;
    true
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AgentSession> {
        self.sessions.lock().unwrap().clone()
    }

    /// Flattened snapshot of all remote-device sessions, for the emit-time
    /// merge in `commands::resolved_snapshot`.
    pub fn remote_snapshot(&self) -> Vec<AgentSession> {
        self.remote.lock().unwrap().values().flat_map(|d| d.sessions.iter().cloned()).collect()
    }

    /// Drop remote devices not heard from within `ttl_ms`. Returns `true`
    /// when anything was dropped (caller re-emits).
    pub fn reap_remote(&self, now_ms: i64, ttl_ms: i64) -> bool {
        let mut remote = self.remote.lock().unwrap();
        let before = remote.len();
        remote.retain(|_, d| now_ms - d.last_seen <= ttl_ms);
        remote.len() != before
    }

    /// Returns `true` when the session's dialog was modified (caller should
    /// persist). The `restored` parameter is used only when creating a new
    /// session to pre-populate dialog + original_prompt + task_started_at
    /// from the persistence store.
    pub fn apply_set(
        &self,
        input: SetInput,
        now_ms: i64,
        continuation_prompts: &[String],
        restored: Option<PersistedSession>,
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let dialog_entry = input.dialog_entry.clone();
        if let Some(existing) = sessions.iter_mut().find(|s| s.id == input.id) {
            let prior = existing.status;

            let raw_task_boundary = matches!(
                prior,
                Status::Done | Status::Idle | Status::Working | Status::Waiting
            ) && input.status == Status::Working;
            let is_continuation = raw_task_boundary
                && input
                    .label
                    .as_deref()
                    .is_some_and(|l| is_continuation_prompt(l, continuation_prompts));
            let task_boundary = raw_task_boundary && !is_continuation;

            if prior == Status::Working && input.status != Status::Working {
                let delta = (now_ms - existing.state_entered_at).max(0) as u64;
                existing.working_accumulated_ms = existing.working_accumulated_ms.saturating_add(delta);
            }

            // Remember where to revert if this turn is cancelled with Esc. Only
            // capture on a real entry into Working (not Working → Working), so
            // the snapshot is always a genuine pre-prompt status — typically the
            // `Blocked` of a question the user is mid-answer to.
            if input.status == Status::Working && prior != Status::Working {
                existing.status_before_working = prior;
            }

            let (new_label, new_original_prompt) =
                crate::label_policy::select(Some(&*existing), &input, task_boundary);

            if task_boundary {
                existing.working_accumulated_ms = 0;
            }

            if prior != input.status || task_boundary {
                existing.state_entered_at = now_ms;
            }

            tracing::debug!(
                id = %input.id,
                decision = "apply_set",
                path = "existing",
                prior_status = ?prior,
                new_status = ?input.status,
                task_boundary,
                continuation_suppressed = is_continuation,
                input_label = ?input.label,
                prior_original_prompt = ?existing.original_prompt,
                new_label = %new_label,
                new_original_prompt = ?new_original_prompt,
                "apply_set"
            );

            if task_boundary
                && new_original_prompt.is_some()
                && new_original_prompt != existing.original_prompt
            {
                existing.task_started_at = now_ms;
            }

            existing.status = input.status;
            existing.label = new_label;
            existing.original_prompt = new_original_prompt;
            if let Some(src) = input.source {
                existing.source = src;
            }
            if input.model.is_some() {
                existing.model = input.model;
            }
            if input.input_tokens.is_some() {
                existing.input_tokens = input.input_tokens;
            }
            existing.updated = now_ms;

            if let Some(pending) = dialog_entry {
                let task_start = pending.role == DialogRole::User && task_boundary;
                existing.dialog.push(DialogEntry {
                    role: pending.role,
                    text: pending.text,
                    timestamp: now_ms,
                    status: existing.status,
                    task_start,
                });
                return true;
            }
            false
        } else {
            let (label, event_prompt) = crate::label_policy::select(None, &input, false);
            tracing::debug!(
                id = %input.id,
                decision = "apply_set",
                path = "new",
                new_status = ?input.status,
                input_label = ?input.label,
                new_label = %label,
                new_original_prompt = ?event_prompt,
                "apply_set"
            );

            let r = restored.unwrap_or_default();
            // A restored dialog ending in a separator means the conversation was
            // cleared or compacted — the boundary marker is the last thing on the
            // row, so no task is in flight. Don't resurrect the pre-boundary
            // task's prompt/timer onto the fresh row (that's what made a `/clear`
            // recreate the row as Idle still showing the previous task). Keep the
            // dialog for history continuity but start the row's active-task state
            // clean. An incoming `event_prompt` (a Working prompt arriving with
            // this same event) still takes precedence and starts a real task.
            let cleared = r.dialog.last().is_some_and(|e| e.role == DialogRole::Separator);
            let restored_prompt = if cleared { None } else { r.original_prompt };
            let restored_task_started_at = if cleared { 0 } else { r.task_started_at };
            let original_prompt = event_prompt.or(restored_prompt);
            let task_started_at = if original_prompt.is_some() && restored_task_started_at == 0 {
                now_ms
            } else {
                restored_task_started_at
            };
            let mut dialog = r.dialog;

            let has_new_entry = if let Some(pending) = dialog_entry {
                let task_start = pending.role == DialogRole::User;
                dialog.push(DialogEntry {
                    role: pending.role,
                    text: pending.text,
                    timestamp: now_ms,
                    status: input.status,
                    task_start,
                });
                true
            } else {
                false
            };

            let dialog_restored = !dialog.is_empty();
            sessions.push(AgentSession {
                id: input.id,
                status: input.status,
                status_before_working: Status::Idle,
                label,
                original_prompt,
                task_started_at,
                dialog,
                source: input.source.unwrap_or_else(|| "claude-code".to_string()),
                model: input.model,
                input_tokens: input.input_tokens,
                updated: now_ms,
                state_entered_at: now_ms,
                working_accumulated_ms: 0,
                display_name: None,
                origin: None,
            });
            has_new_entry || dialog_restored
        }
    }

    /// Remove a session and return it, after appending a session boundary to its
    /// dialog (so a restored copy ends with a separator, exactly like `/clear`).
    /// When `expect_updated` is `Some`, the removal is aborted (returns `None`)
    /// if the row's `updated` no longer matches — used by the liveness reaper to
    /// avoid deleting a row that received a new event between observation and
    /// removal. The check, boundary append, and removal all happen under one
    /// lock, so it is atomic against a concurrent hook event.
    pub fn take_session(&self, id: &str, expect_updated: Option<i64>, now_ms: i64) -> Option<AgentSession> {
        let mut sessions = self.sessions.lock().unwrap();
        let pos = sessions.iter().position(|s| s.id == id)?;
        if let Some(expected) = expect_updated {
            if sessions[pos].updated != expected {
                return None;
            }
        }
        append_boundary(&mut sessions[pos], now_ms);
        Some(sessions.remove(pos))
    }

    /// Revert a `Working` session whose turn was cancelled with Esc back to the
    /// status it held *before* the turn started (`status_before_working`),
    /// rather than blanket-`Idle`. Called by the transcript watcher on the
    /// "[Request interrupted by user]" marker — an Esc emits no lifecycle hook,
    /// so without this the row would stay `Working` forever (and the watcher's
    /// own `infer_state` would
    /// otherwise re-promote the marker as user input). The cancelled turn
    /// produced nothing, so the row should look as if the prompt never landed:
    /// a reply aborted mid-question reverts to `Blocked`, so the user's real
    /// answer is an approval-cycle reply (no task boundary) instead of a fresh
    /// task that clobbers `original_prompt`. No-op unless still `Working`, so a
    /// turn that already moved on is left alone. Mirrors `apply_set`'s
    /// Working→non-Working accounting (banks the elapsed run, resets the timer).
    /// Returns true if it acted.
    /// Returns the status it reverted to (for the decision log), or `None` when
    /// it was a no-op because the row had already left `Working`.
    pub fn revert_cancelled_turn(&self, id: &str, now_ms: i64) -> Option<Status> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.iter_mut().find(|s| s.id == id)?;
        if s.status != Status::Working {
            return None;
        }
        let delta = (now_ms - s.state_entered_at).max(0) as u64;
        s.working_accumulated_ms = s.working_accumulated_ms.saturating_add(delta);
        s.status = s.status_before_working;
        s.state_entered_at = now_ms;
        s.updated = now_ms;
        Some(s.status)
    }

    /// Mark a session-boundary in the in-memory dialog. Called on the
    /// authoritative boundary signals — `SessionEnd` (before `/clear` removes
    /// the row) and `PreCompact` (context compaction) — to append a history
    /// separator without resurrecting the prior task onto the next turn.
    pub fn mark_session_boundary(&self, id: &str, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        append_boundary(session, now_ms)
    }

    /// Watcher-driven text capture. Processes transcript text entries in
    /// chronological order. User entries append (with dedup). Assistant
    /// entries replace the last assistant in the current turn (same-turn
    /// update), or append if a user entry separates them (new turn after
    /// an interrupt).
    pub fn apply_text_entries(&self, id: &str, entries: &[(DialogRole, String)], now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        // The watcher only ever yields User/Assistant; separators enter a
        // dialog via mark_session_boundary, not the transcript.
        let incoming: Vec<DialogEntry> = entries
            .iter()
            .filter(|(role, _)| matches!(role, DialogRole::User | DialogRole::Assistant))
            .map(|(role, text)| DialogEntry { role: *role, text: text.clone(), timestamp: now_ms, status: session.status, task_start: false })
            .collect();
        let changed = merge_dialog_entries(&mut session.dialog, &incoming);
        if changed {
            session.updated = now_ms;
        }
        changed
    }
}

/// Merge `incoming` dialog entries (chronological order) into `dialog` with
/// the turn-aware semantics of the transcript watcher, made replay-safe so
/// the sync receive path can apply overlapping deltas idempotently (a failed
/// push leaves the sender's watermark in place, so the next push re-sends
/// the same entries):
/// - User: append, unless the last user entry has the same text (watcher
///   dedup of re-read transcripts) or an identical entry — same timestamp
///   and text — already exists (replayed delta).
/// - Assistant: replace the tail assistant of the current turn in place
///   (same-turn streaming update — also how a replayed newer version of the
///   same turn lands), skip when its text already matches, append when a
///   user entry intervened.
/// - Separator: append, unless the dialog already ends with one (mirrors the
///   mark_session_boundary guard) or the same separator (by timestamp) was
///   already merged.
/// Returns `true` when the dialog was modified.
pub fn merge_dialog_entries(dialog: &mut Vec<DialogEntry>, incoming: &[DialogEntry]) -> bool {
    let mut changed = false;
    for entry in incoming {
        match entry.role {
            DialogRole::User => {
                let last_user = dialog.iter().rev().find(|e| e.role == DialogRole::User);
                if last_user.is_some_and(|e| e.text == entry.text) {
                    continue;
                }
                if dialog.iter().any(|e| e.role == DialogRole::User && e.timestamp == entry.timestamp && e.text == entry.text) {
                    continue;
                }
                dialog.push(entry.clone());
                changed = true;
            }
            DialogRole::Assistant => {
                let tail_idx = dialog.iter().enumerate().rev()
                    .take_while(|(_, e)| e.role != DialogRole::User)
                    .find(|(_, e)| e.role == DialogRole::Assistant)
                    .map(|(i, _)| i);
                if let Some(i) = tail_idx {
                    if dialog[i].text == entry.text {
                        continue;
                    }
                    dialog[i] = entry.clone();
                } else {
                    dialog.push(entry.clone());
                }
                changed = true;
            }
            DialogRole::Separator => {
                if dialog.last().is_some_and(|e| e.role == DialogRole::Separator) {
                    continue;
                }
                if dialog.iter().any(|e| e.role == DialogRole::Separator && e.timestamp == entry.timestamp) {
                    continue;
                }
                dialog.push(entry.clone());
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: &str, status: Status, label: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: Some(label.to_string()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
        }
    }

    fn set_no_label(id: &str, status: Status) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: None,
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
        }
    }

    fn get<'a>(state: &'a AppState, id: &str) -> AgentSession {
        state
            .snapshot()
            .into_iter()
            .find(|s| s.id == id)
            .expect("session")
    }

    const NO_CONTINUATIONS: &[String] = &[];

    #[test]
    fn revert_cancelled_turn_banks_elapsed_and_falls_back_to_idle() {
        // A fresh session's first turn has no prior status, so a cancel reverts
        // to Idle (status_before_working defaults to Idle).
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);

        assert_eq!(state.revert_cancelled_turn("a", 20_000), Some(Status::Idle));
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Idle);
        assert_eq!(s.working_accumulated_ms, 20_000, "elapsed run banked");
        assert_eq!(s.state_entered_at, 20_000);
        assert_eq!(s.updated, 20_000);
    }

    #[test]
    fn revert_cancelled_turn_restores_blocked_after_aborted_reply() {
        // The reported bug: agent asks a question (Blocked), the user submits a
        // reply (Working) then cancels it with Esc. The row must revert to
        // Blocked — not Idle — so the user's *real* answer is an approval-cycle
        // reply (Blocked → Working, no boundary) and original_prompt survives.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix the parser"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "Push?"), 10_000, NO_CONTINUATIONS, None);
        // User submits a typo'd reply, which enters Working from Blocked...
        state.apply_set(set("a", Status::Working, "ny"), 12_000, NO_CONTINUATIONS, None);
        // ...then cancels it with Esc (no Stop hook) — the watcher calls
        // revert_cancelled_turn on the interrupt marker.
        assert_eq!(state.revert_cancelled_turn("a", 13_000), Some(Status::Blocked));
        let reverted = get(&state, "a");
        assert_eq!(reverted.status, Status::Blocked, "cancelled reply reverts to the pending question");

        // The real answer now lands from Blocked — an approval cycle, not a task
        // boundary — so the task is preserved even without a continuation match.
        state.apply_set(set("a", Status::Working, "y"), 14_000, NO_CONTINUATIONS, None);
        let answered = get(&state, "a");
        assert_eq!(answered.original_prompt.as_deref(), Some("fix the parser"), "answer must not clobber the task");
    }

    #[test]
    fn revert_cancelled_turn_is_noop_when_not_working() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Blocked, "run bash?"), 0, NO_CONTINUATIONS, None);
        assert_eq!(state.revert_cancelled_turn("a", 20_000), None);
        assert_eq!(get(&state, "a").status, Status::Blocked);
    }

    #[test]
    fn new_working_session_captures_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 1000, NO_CONTINUATIONS, None);

        let s = get(&state, "a");
        assert_eq!(s.status, Status::Working);
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(s.state_entered_at, 1000);
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn new_non_working_session_has_no_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Idle, ""), 1000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").original_prompt, None);
    }

    #[test]
    fn approval_cycle_preserves_original_prompt_and_accumulator() {
        let state = AppState::new();
        // Initial working: task starts
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        // Claude asks for approval after 30s
        state.apply_set(set("a", Status::Blocked, "run bash?"), 30_000, NO_CONTINUATIONS, None);
        let mid = get(&state, "a");
        assert_eq!(mid.status, Status::Blocked);
        assert_eq!(mid.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(mid.working_accumulated_ms, 30_000);
        assert_eq!(mid.state_entered_at, 30_000);

        // User approves after 5s; agent resumes working with noise label "yes"
        state.apply_set(set("a", Status::Working, "yes"), 35_000, NO_CONTINUATIONS, None);
        let resumed = get(&state, "a");
        assert_eq!(resumed.status, Status::Working);
        assert_eq!(
            resumed.original_prompt.as_deref(),
            Some("fix foo.py"),
            "original prompt must survive approval cycle"
        );
        assert_eq!(
            resumed.working_accumulated_ms, 30_000,
            "accumulated time from before the approval must be preserved"
        );
        assert_eq!(resumed.state_entered_at, 35_000);
    }

    #[test]
    fn done_then_working_is_task_boundary_and_resets_state() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "fixed!"), 60_000, NO_CONTINUATIONS, None);
        let after_done = get(&state, "a");
        assert_eq!(
            after_done.working_accumulated_ms, 60_000,
            "working time accumulated on exit"
        );
        assert_eq!(after_done.original_prompt.as_deref(), Some("fix foo.py"));

        // New task on the same session
        state.apply_set(set("a", Status::Working, "add tests"), 120_000, NO_CONTINUATIONS, None);
        let new_task = get(&state, "a");
        assert_eq!(new_task.original_prompt.as_deref(), Some("add tests"));
        assert_eq!(new_task.working_accumulated_ms, 0);
        assert_eq!(new_task.state_entered_at, 120_000);
    }

    #[test]
    fn idle_then_working_is_also_task_boundary() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Idle, ""), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "new task"), 10_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("new task"));
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn working_to_error_accumulates_but_does_not_reset() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "do a thing"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Error, "perm denied"), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Error);
        assert_eq!(s.working_accumulated_ms, 5_000);
        assert_eq!(s.original_prompt.as_deref(), Some("do a thing"));
        assert_eq!(s.label, "perm denied");
    }

    #[test]
    fn same_non_working_status_update_keeps_state_entered_at() {
        // For non-Working same-status updates (e.g. successive Blocked events
        // refining the question), state_entered_at must not bounce. Working →
        // Working is now a task boundary on purpose — see the cancellation tests.
        let state = AppState::new();
        state.apply_set(set("a", Status::Blocked, "ask"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "ask"), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.state_entered_at, 0, "state_entered_at must not reset within the same non-Working status");
    }

    #[test]
    fn working_to_working_with_new_prompt_is_task_boundary() {
        // Cancellation case: user hits Esc mid-task and submits a new prompt
        // without an intervening Stop, so the row never leaves Working. The
        // new prompt must be treated as a fresh task: original_prompt
        // re-captured, working timer reset.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "second task"), 30_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("second task"));
        assert_eq!(s.working_accumulated_ms, 0, "task boundary zeroes the accumulator");
        assert_eq!(s.state_entered_at, 30_000, "task boundary resets segment start even when status is unchanged");
    }

    #[test]
    fn working_to_working_continuation_prompt_does_not_reset() {
        // Even when the prior status is Working, a continuation prompt must
        // suppress the boundary so original_prompt and the timer are preserved.
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo"), 0, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 5_000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.state_entered_at, 0, "continuation suppresses segment-start reset");
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn take_session_removes_and_returns_the_row() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("b", Status::Working, "other"), 0, NO_CONTINUATIONS, None);
        let removed = state.take_session("a", None, 0);
        assert!(removed.is_some(), "the removed session is returned");
        assert_eq!(removed.unwrap().id, "a");
        let ids: Vec<String> = state.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn take_session_aborts_when_updated_moved() {
        // The reaper passes the last-seen `updated`; if an event bumped it
        // between observation and removal, take_session must not delete the row.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        let updated = get(&state, "a").updated;
        assert!(state.take_session("a", Some(updated + 1), 0).is_none(), "stale expectation aborts");
        assert_eq!(state.snapshot().len(), 1, "row survives a mismatched expectation");
        assert!(state.take_session("a", Some(updated), 0).is_some(), "matching expectation removes");
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn take_session_appends_boundary_before_removing() {
        // A removed dialog should end with a separator so a restored copy starts
        // a clean task (same continuity /clear relies on).
        let state = AppState::new();
        let mut input = set("a", Status::Working, "task");
        input.dialog_entry = Some(PendingDialogEntry { role: DialogRole::User, text: "task".into() });
        state.apply_set(input, 0, NO_CONTINUATIONS, None);
        let removed = state.take_session("a", None, 100).expect("removed");
        assert_eq!(removed.dialog.last().map(|e| e.role), Some(DialogRole::Separator));
    }

    #[test]
    fn model_and_tokens_are_updated_when_provided() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(
            SetInput {
                id: "a".into(),
                status: Status::Working,
                label: Some("task".into()),
                source: None,
                model: Some("claude-opus-4-7".into()),
                input_tokens: Some(50_000),
                dialog_entry: None,
            },
            1000,
            NO_CONTINUATIONS,
            None,
        );
        let s = get(&state, "a");
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.input_tokens, Some(50_000));
    }

    #[test]
    fn missing_label_preserves_prior_label() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Blocked), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.label, "fix foo.py", "label must survive a set with no label field");
        assert_eq!(s.status, Status::Blocked);
    }

    #[test]
    fn task_boundary_with_missing_label_preserves_prior_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 10_000, NO_CONTINUATIONS, None);
        // New task starts, but hook didn't send a prompt label (e.g. prompt
        // wasn't captured) — original_prompt should remain whatever it was.
        state.apply_set(set_no_label("a", Status::Working), 20_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(s.working_accumulated_ms, 0, "still resets accumulator on task boundary");
    }

    #[test]
    fn continuation_prompt_after_done_does_not_reset_task() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into(), "continue".into(), "proceed".into()];
        // Original task
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        // Agent finishes
        state.apply_set(set_no_label("a", Status::Done), 60_000, &cont, None);
        let after_done = get(&state, "a");
        assert_eq!(after_done.working_accumulated_ms, 60_000);
        assert_eq!(after_done.original_prompt.as_deref(), Some("fix foo.py"));
        // User types "go" — should be treated as a continuation, not a new task
        state.apply_set(set("a", Status::Working, "go"), 80_000, &cont, None);
        let resumed = get(&state, "a");
        assert_eq!(
            resumed.original_prompt.as_deref(),
            Some("fix foo.py"),
            "continuation prompt must NOT re-capture original_prompt"
        );
        assert_eq!(
            resumed.working_accumulated_ms, 60_000,
            "continuation prompt must NOT reset the working timer"
        );
        assert_eq!(resumed.label, "go");
    }

    #[test]
    fn default_affirmations_do_not_clobber_task_after_done() {
        // End-to-end guard against the recurring "row shows 'y' as the task"
        // bug: an approval reply can arrive when the row is Done or Idle rather
        // than Blocked — e.g. the user cancelled a mis-typed reply with Esc
        // (the watcher reverts the turn via the interrupt marker), then typed the
        // real "y". From
        // Done/Idle that "y" would be a task boundary; with the default
        // continuation list it must preserve original_prompt instead.
        let cont = crate::config::Config::default().continuation_prompts;
        for reply in ["y", "yes", "yeah", "yep", "yup", "ok", "okay", "sure", "Yes", " y "] {
            let state = AppState::new();
            state.apply_set(set("a", Status::Working, "fix the parser"), 0, &cont, None);
            state.apply_set(set_no_label("a", Status::Done), 10_000, &cont, None);
            state.apply_set(set("a", Status::Working, reply), 20_000, &cont, None);
            let s = get(&state, "a");
            assert_eq!(s.original_prompt.as_deref(), Some("fix the parser"), "reply {reply:?} clobbered the task");
            assert_eq!(s.working_accumulated_ms, 10_000, "reply {reply:?} reset the timer");
        }
    }

    #[test]
    fn continuation_match_is_case_insensitive_and_trimmed() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into(), "Continue".into()];
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 1000, &cont, None);
        // Match against "Go" (uppercase) and surrounding whitespace
        state.apply_set(set("a", Status::Working, "  Go  "), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
    }

    #[test]
    fn non_continuation_prompt_after_done_still_resets() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 1000, &cont, None);
        // "go ahead" is NOT in the list — exact match only
        state.apply_set(set("a", Status::Working, "go ahead"), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(
            s.original_prompt.as_deref(),
            Some("go ahead"),
            "non-exact-match prompt should re-capture as a fresh task"
        );
    }

    #[test]
    fn task_boundary_updates_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 30_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "second task"), 60_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("second task"));
        assert_eq!(s.task_started_at, 60_000);
    }

    #[test]
    fn approval_cycle_preserves_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "permission?"), 5_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "yes"), 6_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.task_started_at, 0, "task_started_at survives the approval cycle");
    }

    #[test]
    fn continuation_prompt_preserves_task_started_at() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 10_000, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 20_000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.task_started_at, 0, "continuation preserves task_started_at");
    }

    #[test]
    fn first_working_prompt_sets_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.task_started_at, 1_000);
    }

    #[test]
    fn boundary_with_missing_label_preserves_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first"), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 5_000, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Working), 10_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("first"));
        assert_eq!(s.task_started_at, 1_000, "preserved when prompt is preserved");
    }

    // ----- dialog entry creation -----

    fn set_with_dialog(id: &str, status: Status, label: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: Some(label.to_string()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: Some(PendingDialogEntry {
                role: DialogRole::User,
                text: label.to_string(),
            }),
        }
    }

    fn stop_with_dialog(id: &str, status: Status, agent_text: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: None,
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: Some(PendingDialogEntry {
                role: DialogRole::Assistant,
                text: agent_text.to_string(),
            }),
        }
    }

    #[test]
    fn dialog_entry_pushed_for_user_prompt() {
        let state = AppState::new();
        state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::User);
        assert_eq!(s.dialog[0].text, "fix foo");
        assert_eq!(s.dialog[0].timestamp, 1_000);
        assert_eq!(s.dialog[0].status, Status::Working);
    }

    #[test]
    fn dialog_entry_pushed_for_assistant_stop() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        state.apply_set(stop_with_dialog("a", Status::Done, "All fixed."), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::Assistant);
        assert_eq!(s.dialog[0].text, "All fixed.");
        assert_eq!(s.dialog[0].status, Status::Done);
    }

    #[test]
    fn dialog_entry_task_start_marks_only_boundaries() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        // First prompt creates the session — a task start.
        state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 0, &cont, None);
        // Agent finishes.
        state.apply_set(stop_with_dialog("a", Status::Done, "done"), 1_000, &cont, None);
        // Continuation "go" after done — resumes the task, not a new one.
        state.apply_set(set_with_dialog("a", Status::Working, "go"), 2_000, &cont, None);
        // A genuinely new top-level prompt — a task start again.
        state.apply_set(set_with_dialog("a", Status::Working, "next task"), 3_000, &cont, None);
        let s = get(&state, "a");
        let users: Vec<&DialogEntry> = s.dialog.iter().filter(|e| e.role == DialogRole::User).collect();
        assert_eq!(users[0].text, "fix foo");
        assert!(users[0].task_start, "first prompt is a task start");
        assert_eq!(users[1].text, "go");
        assert!(!users[1].task_start, "continuation is not a task start");
        assert_eq!(users[2].text, "next task");
        assert!(users[2].task_start, "new prompt after working is a task start");
        let assistant = s.dialog.iter().find(|e| e.role == DialogRole::Assistant).unwrap();
        assert!(!assistant.task_start, "assistant entries are never task starts");
    }

    #[test]
    fn dialog_not_pushed_without_pending_entry() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert!(s.dialog.is_empty());
    }

    #[test]
    fn dialog_restored_on_new_session() {
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true },
                DialogEntry { role: DialogRole::Assistant, text: "Done.".into(), timestamp: 200, status: Status::Done, task_start: false },
            ],
            original_prompt: Some("old task".into()),
            task_started_at: 100,
        };
        state.apply_set(set("a", Status::Done, "done"), 1_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
        assert_eq!(s.original_prompt.as_deref(), Some("old task"));
        assert_eq!(s.task_started_at, 100);
    }

    #[test]
    fn cleared_session_restore_keeps_dialog_but_drops_task() {
        // After `/clear`: SessionEnd marks a boundary separator + persists, then
        // SessionStart recreates the row from prompt_history with an Idle, no-label
        // Set. The restored dialog ends with the separator, so the row must come
        // back clean — no resurrected original_prompt — while keeping the history.
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true },
                DialogEntry { role: DialogRole::Assistant, text: "Done.".into(), timestamp: 200, status: Status::Done, task_start: false },
                DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 300, status: Status::Idle, task_start: false },
            ],
            original_prompt: Some("old task".into()),
            task_started_at: 100,
        };
        state.apply_set(set_no_label("a", Status::Idle), 1_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Idle);
        assert_eq!(s.original_prompt, None, "cleared row must not show the previous task");
        assert_eq!(s.task_started_at, 0, "cleared row starts with no task timer");
        assert_eq!(s.dialog.len(), 3, "dialog history is preserved for the history window");
    }

    #[test]
    fn cleared_session_restore_still_honors_incoming_prompt() {
        // If a Working prompt arrives on the same event that recreates a cleared
        // session, that's a genuine new task — it must win over the cleared state.
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true },
                DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 300, status: Status::Idle, task_start: false },
            ],
            original_prompt: Some("old task".into()),
            task_started_at: 100,
        };
        state.apply_set(set("a", Status::Working, "new task"), 2_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("new task"));
        assert_eq!(s.task_started_at, 2_000);
    }

    #[test]
    fn apply_set_returns_true_when_dialog_changes() {
        let state = AppState::new();
        let changed = state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        assert!(changed);
        let not_changed = state.apply_set(set("a", Status::Blocked, "question?"), 1_000, NO_CONTINUATIONS, None);
        assert!(!not_changed);
    }

    fn user_entry(text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::User, text: text.into(), timestamp: ts, status: Status::Working, task_start: true }
    }
    fn assistant_entry(text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::Assistant, text: text.into(), timestamp: ts, status: Status::Done, task_start: false }
    }
    fn separator_entry(ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: ts, status: Status::Idle, task_start: false }
    }

    fn seed(state: &AppState, dialog: Vec<DialogEntry>) {
        state.sessions.lock().unwrap().push(AgentSession {
            id: "a".into(),
            status: Status::Done,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog,
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            display_name: None,
            origin: None,
        });
    }

    #[test]
    fn apply_text_appends_assistant_when_empty() {
        let state = AppState::new();
        seed(&state, vec![]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "first".into())], 100);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::Assistant);
        assert_eq!(s.dialog[0].text, "first");
    }

    #[test]
    fn apply_text_appends_after_user() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "answer".into())], 20);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
        assert_eq!(s.dialog[1].text, "answer");
    }

    #[test]
    fn apply_text_replaces_assistant_in_same_turn() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10), assistant_entry("partial", 20)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "full".into())], 30);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2, "replaced in place, not appended");
        assert_eq!(s.dialog[1].text, "full");
    }

    #[test]
    fn apply_text_no_op_when_unchanged() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10), assistant_entry("same", 20)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "same".into())], 30);
        assert!(!changed);
    }

    #[test]
    fn apply_text_interrupt_appends_after_user_boundary() {
        let state = AppState::new();
        seed(&state, vec![user_entry("task", 10)]);
        let entries = vec![
            (DialogRole::User, "interrupt".into()),
            (DialogRole::Assistant, "ack + pivot".into()),
            (DialogRole::Assistant, "final answer".into()),
        ];
        let changed = state.apply_text_entries("a", &entries, 50);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 3);
        assert_eq!(s.dialog[1].role, DialogRole::User);
        assert_eq!(s.dialog[1].text, "interrupt");
        assert_eq!(s.dialog[2].text, "final answer", "same-turn assistant texts replace");
    }

    #[test]
    fn apply_text_dedup_user_from_hook() {
        let state = AppState::new();
        seed(&state, vec![user_entry("fix bug", 10)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::User, "fix bug".into())], 20);
        assert!(!changed, "hook already captured this prompt");
    }

    #[test]
    fn apply_text_missing_session_is_noop() {
        let state = AppState::new();
        assert!(!state.apply_text_entries("nope", &[(DialogRole::Assistant, "x".into())], 0));
    }

    #[test]
    fn mark_session_boundary_appends_separator() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), assistant_entry("a1", 20)]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 3);
        assert_eq!(s.dialog[2].role, DialogRole::Separator);
        assert_eq!(s.dialog[2].timestamp, 100);
        assert_eq!(s.updated, 100);
    }

    #[test]
    fn mark_session_boundary_noop_on_empty_dialog() {
        let state = AppState::new();
        seed(&state, vec![]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(!changed);
        let s = get(&state, "a");
        assert!(s.dialog.is_empty());
    }

    #[test]
    fn mark_session_boundary_idempotent_on_trailing_separator() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), separator_entry(20)]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(!changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
    }

    #[test]
    fn mark_session_boundary_missing_session_is_noop() {
        let state = AppState::new();
        assert!(!state.mark_session_boundary("nope", 100));
    }

    #[test]
    fn continuation_only_applies_to_task_boundary_transitions() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        // Existing approval cycle: blocked → working with label "go".
        // This isn't a task boundary regardless of the continuation list,
        // so the rule is a no-op here — original_prompt is still pinned.
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set("a", Status::Blocked, "permission?"), 1000, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
    }

    // -------- merge_dialog_entries (sync delta path) tests --------

    #[test]
    fn merge_replay_of_same_delta_is_noop() {
        let mut dialog = Vec::new();
        let delta = vec![user_entry("u1", 10), assistant_entry("a1", 20), separator_entry(30)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 3);
        // A failed push re-sends the same window — must not duplicate.
        assert!(!merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 3);
    }

    #[test]
    fn merge_replaces_streamed_assistant_in_place() {
        let mut dialog = vec![user_entry("u1", 10), assistant_entry("partial", 20)];
        // The origin's watcher rewrote the same-turn assistant text and
        // bumped its timestamp; the delta carries the newer version.
        let delta = vec![assistant_entry("final", 25)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 2);
        assert_eq!(dialog[1].text, "final");
        assert_eq!(dialog[1].timestamp, 25);
    }

    #[test]
    fn merge_appends_assistant_after_user_boundary() {
        let mut dialog = vec![user_entry("u1", 10), assistant_entry("a1", 20)];
        let delta = vec![user_entry("u2", 30), assistant_entry("a2", 40)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 4);
        assert_eq!(dialog[3].text, "a2");
    }

    #[test]
    fn merge_separator_skips_when_dialog_ends_with_one() {
        let mut dialog = vec![user_entry("u1", 10), separator_entry(20)];
        assert!(!merge_dialog_entries(&mut dialog, &[separator_entry(50)]));
        assert_eq!(dialog.len(), 2);
    }

    #[test]
    fn merge_user_dedups_against_last_user() {
        let mut dialog = vec![user_entry("fix bug", 10), assistant_entry("done", 20)];
        // Same prompt re-read with a different timestamp (transcript re-read
        // on the origin) — text dedup against the last user entry catches it.
        assert!(!merge_dialog_entries(&mut dialog, &[user_entry("fix bug", 30)]));
        assert_eq!(dialog.len(), 2);
    }

    #[test]
    fn merge_preserves_incoming_metadata() {
        let mut dialog = Vec::new();
        let mut entry = user_entry("u1", 42);
        entry.task_start = true;
        assert!(merge_dialog_entries(&mut dialog, &[entry]));
        assert_eq!(dialog[0].timestamp, 42, "sender timestamps survive");
        assert!(dialog[0].task_start, "task boundary flag survives");
    }

    // -------- remote-device storage tests --------

    fn remote_session(id: &str, origin: &str) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            display_name: None,
            origin: Some(origin.to_string()),
        }
    }

    #[test]
    fn remote_snapshot_is_ordered_by_device_name() {
        let state = AppState::new();
        let mut remote = state.remote.lock().unwrap();
        remote.insert("zeta".into(), RemoteDevice { sessions: vec![remote_session("zeta/p", "zeta")], last_seen: 0, origin_addr: String::new() });
        remote.insert("alpha".into(), RemoteDevice { sessions: vec![remote_session("alpha/p", "alpha")], last_seen: 0, origin_addr: String::new() });
        drop(remote);
        let snap = state.remote_snapshot();
        let ids: Vec<&str> = snap.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha/p", "zeta/p"], "stable order across emits");
    }

    #[test]
    fn reap_remote_drops_only_silent_devices() {
        let state = AppState::new();
        let mut remote = state.remote.lock().unwrap();
        remote.insert("fresh".into(), RemoteDevice { sessions: Vec::new(), last_seen: 1000, origin_addr: String::new() });
        remote.insert("stale".into(), RemoteDevice { sessions: Vec::new(), last_seen: 0, origin_addr: String::new() });
        drop(remote);
        assert!(state.reap_remote(1500, 1000), "stale device dropped");
        assert!(state.remote.lock().unwrap().contains_key("fresh"));
        assert!(!state.remote.lock().unwrap().contains_key("stale"));
        assert!(!state.reap_remote(1500, 1000), "second reap is a no-op");
    }
}
