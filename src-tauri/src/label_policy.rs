//! Sticky-label policy: decides what `label` and `original_prompt` a session
//! should carry after an incoming [`SetInput`] is applied.
//!
//! This module is intentionally trivial today — it exists as a dedicated seam
//! for the sticky-label refactor. All the policy decisions that used to live
//! inline in `state::AppState::apply_set` now route through
//! [`select`] without changing observable behavior.

use crate::state::{AgentSession, SetInput, Status};

/// What a session carries after an update: its label and its task.
#[derive(Debug, PartialEq, Eq)]
pub struct Selected {
    pub label: String,
    pub original_prompt: Option<String>,
    /// Follows `original_prompt` exactly: taken from the input whenever
    /// `original_prompt` is, and kept from the previous state whenever it is
    /// kept, so it always describes the task `original_prompt` holds.
    pub delegated_task: Option<String>,
    /// Follows `original_prompt` exactly, as `delegated_task` does.
    pub message_line: Option<String>,
}

/// Decide the post-update label and task for a session.
///
/// - `prev`: the session's state before the update (`None` if this is a brand
///   new session).
/// - `input`: the incoming [`SetInput`].
/// - `task_boundary`: whether this update represents a fresh task starting
///   (prior status was `Done`/`Idle`/`Working`/`Waiting` and the new status is
///   `Working`, unless the label is a continuation phrase, the event is a
///   prompt that names no task, or it is another agent's message arriving while
///   the turn is still `Working`/`Waiting` or a relayed reply to one this row's
///   agent sent). For a new session this is ignored —
///   new-session rules apply instead.
///
/// Rules (preserved verbatim from the original inline implementation):
///
/// **New session (`prev = None`):**
/// - `label` = `input.label` or `""`
/// - `original_prompt` = `Some(label)` iff entering `Working` with a non-empty
///   label, else `None`.
///
/// **Existing session (`prev = Some(p)`):**
/// - `label` = `input.label` if provided, else `p.label` (preserved).
/// - `original_prompt`:
///   - on task boundary, captured from `input.label` if provided, else
///     `p.original_prompt` is preserved.
///   - off task boundary, `p.original_prompt` is always preserved.
///
/// `delegated_task` and `message_line` are captured from the input's wherever
/// `original_prompt` is captured from `input.label`, and otherwise carried over
/// with it (`None` on a new session that captures no prompt).
pub fn select(
    prev: Option<&AgentSession>,
    input: &SetInput,
    task_boundary: bool,
) -> Selected {
    match prev {
        None => {
            let label = input.label.clone().unwrap_or_default();
            let captured = input.status == Status::Working && !label.is_empty();
            let original_prompt = captured.then(|| label.clone());
            let delegated_task = input.delegated_task.clone().filter(|_| captured);
            let message_line = input.message_line.clone().filter(|_| captured);
            Selected { label, original_prompt, delegated_task, message_line }
        }
        Some(p) => {
            let label = input.label.clone().unwrap_or_else(|| p.label.clone());
            let (original_prompt, delegated_task, message_line) = match &input.label {
                Some(l) if task_boundary => (Some(l.clone()), input.delegated_task.clone(), input.message_line.clone()),
                _ => (p.original_prompt.clone(), p.delegated_task.clone(), p.message_line.clone()),
            };
            Selected { label, original_prompt, delegated_task, message_line }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(status: Status, label: Option<&str>) -> SetInput {
        SetInput {
            id: "a".into(),
            status,
            label: label.map(str::to_string),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        }
    }

    fn session(label: &str, original_prompt: Option<&str>) -> AgentSession {
        AgentSession {
            id: "a".into(),
            status: Status::Idle,
            status_before_working: Status::Idle,
            label: label.into(),
            original_prompt: original_prompt.map(str::to_string),
            task_started_at: 0,
            dialog: Vec::new(),
            source: "claude-code".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: None,
            instruction_drift: false,
            canary: crate::state::Canary::Off,
            attended_at: None,
            turn_from_relay: false,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            subagent_gate: None,
            terminal_stale_at: None,
        }
    }

    #[test]
    fn new_working_with_label_captures_original_prompt() {
        let Selected { label, original_prompt: op, .. } = select(None, &input(Status::Working, Some("fix foo")), true);
        assert_eq!(label, "fix foo");
        assert_eq!(op.as_deref(), Some("fix foo"));
    }

    #[test]
    fn new_working_without_label_has_no_original_prompt() {
        let Selected { label, original_prompt: op, .. } = select(None, &input(Status::Working, None), true);
        assert_eq!(label, "");
        assert_eq!(op, None);
    }

    #[test]
    fn new_working_with_empty_label_has_no_original_prompt() {
        let Selected { label, original_prompt: op, .. } = select(None, &input(Status::Working, Some("")), true);
        assert_eq!(label, "");
        assert_eq!(op, None);
    }

    #[test]
    fn new_non_working_never_has_original_prompt() {
        let Selected { original_prompt: op, .. } = select(None, &input(Status::Idle, Some("foo")), false);
        assert_eq!(op, None);
    }

    #[test]
    fn existing_non_boundary_with_label_overwrites_label_but_preserves_original() {
        let prev = session("prior", Some("original"));
        let Selected { label, original_prompt: op, .. } = select(
            Some(&prev),
            &input(Status::Blocked, Some("new label")),
            false,
        );
        assert_eq!(label, "new label");
        assert_eq!(op.as_deref(), Some("original"));
    }

    #[test]
    fn existing_non_boundary_without_label_preserves_both() {
        let prev = session("prior", Some("original"));
        let Selected { label, original_prompt: op, .. } = select(Some(&prev), &input(Status::Blocked, None), false);
        assert_eq!(label, "prior");
        assert_eq!(op.as_deref(), Some("original"));
    }

    #[test]
    fn existing_boundary_with_label_captures_new_original() {
        let prev = session("prior", Some("original"));
        let Selected { label, original_prompt: op, .. } = select(Some(&prev), &input(Status::Working, Some("new task")), true);
        assert_eq!(label, "new task");
        assert_eq!(op.as_deref(), Some("new task"));
    }

    #[test]
    fn existing_boundary_without_label_preserves_prior_original() {
        let prev = session("prior", Some("original"));
        let Selected { label, original_prompt: op, .. } = select(Some(&prev), &input(Status::Working, None), true);
        assert_eq!(
            label, "prior",
            "missing label should fall back to prior label"
        );
        assert_eq!(
            op.as_deref(),
            Some("original"),
            "missing label on boundary must not clobber original_prompt"
        );
    }

    #[test]
    fn delegated_task_is_captured_and_kept_exactly_with_original_prompt() {
        let with = |status, label| SetInput { delegated_task: Some("the sender's task".into()), ..input(status, label) };
        assert_eq!(select(None, &with(Status::Working, Some("<envelope>")), true).delegated_task.as_deref(), Some("the sender's task"));
        assert_eq!(select(None, &with(Status::Idle, Some("<envelope>")), false).delegated_task, None, "no prompt captured, nothing to describe");

        let mut prev = session("prior", Some("<old envelope>"));
        prev.delegated_task = Some("old task".into());
        assert_eq!(select(Some(&prev), &with(Status::Working, Some("<envelope>")), true).delegated_task.as_deref(), Some("the sender's task"), "a new task brings its own");
        assert_eq!(select(Some(&prev), &with(Status::Working, Some("<envelope>")), false).delegated_task.as_deref(), Some("old task"), "off a boundary the task, and so this, is kept");
        assert_eq!(select(Some(&prev), &with(Status::Working, None), true).delegated_task.as_deref(), Some("old task"), "no label, no new prompt");
        assert_eq!(select(Some(&prev), &input(Status::Working, Some("typed")), true).delegated_task, None, "a person's prompt clears it");

        let line = |status, label| SetInput { message_line: Some("the message's first line".into()), ..input(status, label) };
        assert_eq!(select(None, &line(Status::Working, Some("<envelope>")), true).message_line.as_deref(), Some("the message's first line"));
        prev.message_line = Some("old line".into());
        assert_eq!(select(Some(&prev), &line(Status::Working, Some("<envelope>")), false).message_line.as_deref(), Some("old line"), "kept off a boundary");
        assert_eq!(select(Some(&prev), &input(Status::Working, Some("typed")), true).message_line, None, "a person's prompt clears it");
    }
}
