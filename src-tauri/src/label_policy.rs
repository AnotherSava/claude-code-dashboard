//! Sticky-label policy: decides what `label` and `original_prompt` a session
//! should carry after an incoming [`SetInput`] is applied.
//!
//! This module is intentionally trivial today — it exists as a dedicated seam
//! for the sticky-label refactor. All the policy decisions that used to live
//! inline in `state::AppState::apply_set` now route through
//! [`select`] without changing observable behavior.

use crate::state::{AgentSession, SetInput, Status};

/// Decide the post-update `(label, original_prompt)` pair for a session.
///
/// - `prev`: the session's state before the update (`None` if this is a brand
///   new session).
/// - `input`: the incoming [`SetInput`].
/// - `task_boundary`: whether this update represents a fresh task starting
///   (prior status was `Done`/`Idle` and the new status is `Working`). For a
///   new session this is ignored — new-session rules apply instead.
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
pub fn select(
    prev: Option<&AgentSession>,
    input: &SetInput,
    task_boundary: bool,
) -> (String, Option<String>) {
    match prev {
        None => {
            let label = input.label.clone().unwrap_or_default();
            let original_prompt = if input.status == Status::Working && !label.is_empty() {
                Some(label.clone())
            } else {
                None
            };
            (label, original_prompt)
        }
        Some(p) => {
            let label = input.label.clone().unwrap_or_else(|| p.label.clone());
            let original_prompt = if task_boundary {
                if let Some(ref l) = input.label {
                    Some(l.clone())
                } else {
                    p.original_prompt.clone()
                }
            } else {
                p.original_prompt.clone()
            };
            (label, original_prompt)
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
        }
    }

    fn session(label: &str, original_prompt: Option<&str>) -> AgentSession {
        AgentSession {
            id: "a".into(),
            status: Status::Idle,
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
        }
    }

    #[test]
    fn new_working_with_label_captures_original_prompt() {
        let (label, op) = select(None, &input(Status::Working, Some("fix foo")), true);
        assert_eq!(label, "fix foo");
        assert_eq!(op.as_deref(), Some("fix foo"));
    }

    #[test]
    fn new_working_without_label_has_no_original_prompt() {
        let (label, op) = select(None, &input(Status::Working, None), true);
        assert_eq!(label, "");
        assert_eq!(op, None);
    }

    #[test]
    fn new_working_with_empty_label_has_no_original_prompt() {
        let (label, op) = select(None, &input(Status::Working, Some("")), true);
        assert_eq!(label, "");
        assert_eq!(op, None);
    }

    #[test]
    fn new_non_working_never_has_original_prompt() {
        let (_, op) = select(None, &input(Status::Idle, Some("foo")), false);
        assert_eq!(op, None);
    }

    #[test]
    fn existing_non_boundary_with_label_overwrites_label_but_preserves_original() {
        let prev = session("prior", Some("original"));
        let (label, op) = select(
            Some(&prev),
            &input(Status::Awaiting, Some("new label")),
            false,
        );
        assert_eq!(label, "new label");
        assert_eq!(op.as_deref(), Some("original"));
    }

    #[test]
    fn existing_non_boundary_without_label_preserves_both() {
        let prev = session("prior", Some("original"));
        let (label, op) = select(Some(&prev), &input(Status::Awaiting, None), false);
        assert_eq!(label, "prior");
        assert_eq!(op.as_deref(), Some("original"));
    }

    #[test]
    fn existing_boundary_with_label_captures_new_original() {
        let prev = session("prior", Some("original"));
        let (label, op) = select(Some(&prev), &input(Status::Working, Some("new task")), true);
        assert_eq!(label, "new task");
        assert_eq!(op.as_deref(), Some("new task"));
    }

    #[test]
    fn existing_boundary_without_label_preserves_prior_original() {
        let prev = session("prior", Some("original"));
        let (label, op) = select(Some(&prev), &input(Status::Working, None), true);
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
}
