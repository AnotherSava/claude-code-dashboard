use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Working,
    Awaiting,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DialogEntry {
    pub role: DialogRole,
    pub text: String,
    pub timestamp: i64,
    pub status: Status,
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

#[derive(Default)]
pub struct AppState {
    pub sessions: Mutex<Vec<AgentSession>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AgentSession> {
        self.sessions.lock().unwrap().clone()
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
                Status::Done | Status::Idle | Status::Working
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
                existing.dialog.push(DialogEntry {
                    role: pending.role,
                    text: pending.text,
                    timestamp: now_ms,
                    status: existing.status,
                });
                return true;
            }
            false
        } else {
            let (label, event_prompt) = crate::label_policy::select(None, &input, false);
            tracing::debug!(
                id = %input.id,
                path = "new",
                new_status = ?input.status,
                input_label = ?input.label,
                new_label = %label,
                new_original_prompt = ?event_prompt,
                "apply_set"
            );

            let r = restored.unwrap_or_default();
            let original_prompt = event_prompt.or(r.original_prompt);
            let task_started_at = if original_prompt.is_some() && r.task_started_at == 0 {
                now_ms
            } else {
                r.task_started_at
            };
            let mut dialog = r.dialog;

            let has_new_entry = if let Some(pending) = dialog_entry {
                dialog.push(DialogEntry {
                    role: pending.role,
                    text: pending.text,
                    timestamp: now_ms,
                    status: input.status,
                });
                true
            } else {
                false
            };

            let dialog_restored = !dialog.is_empty();
            sessions.push(AgentSession {
                id: input.id,
                status: input.status,
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
            });
            has_new_entry || dialog_restored
        }
    }

    pub fn apply_clear(&self, id: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|s| s.id != id);
    }
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
        state.apply_set(set("a", Status::Awaiting, "run bash?"), 30_000, NO_CONTINUATIONS, None);
        let mid = get(&state, "a");
        assert_eq!(mid.status, Status::Awaiting);
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
        // For non-Working same-status updates (e.g. successive Awaiting events
        // refining the question), state_entered_at must not bounce. Working →
        // Working is now a task boundary on purpose — see the cancellation tests.
        let state = AppState::new();
        state.apply_set(set("a", Status::Awaiting, "ask"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Awaiting, "ask"), 5_000, NO_CONTINUATIONS, None);
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
    fn clear_removes_session() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("b", Status::Working, "other"), 0, NO_CONTINUATIONS, None);
        state.apply_clear("a");
        let ids: Vec<String> = state.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["b"]);
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
        state.apply_set(set_no_label("a", Status::Awaiting), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.label, "fix foo.py", "label must survive a set with no label field");
        assert_eq!(s.status, Status::Awaiting);
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
        state.apply_set(set("a", Status::Awaiting, "permission?"), 5_000, NO_CONTINUATIONS, None);
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
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working },
                DialogEntry { role: DialogRole::Assistant, text: "Done.".into(), timestamp: 200, status: Status::Done },
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
    fn apply_set_returns_true_when_dialog_changes() {
        let state = AppState::new();
        let changed = state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        assert!(changed);
        let not_changed = state.apply_set(set("a", Status::Awaiting, "question?"), 1_000, NO_CONTINUATIONS, None);
        assert!(!not_changed);
    }

    #[test]
    fn continuation_only_applies_to_task_boundary_transitions() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        // Existing approval cycle: awaiting → working with label "go".
        // This isn't a task boundary regardless of the continuation list,
        // so the rule is a no-op here — original_prompt is still pinned.
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set("a", Status::Awaiting, "permission?"), 1000, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
    }
}
