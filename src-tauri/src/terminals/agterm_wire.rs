//! agterm's `session context` protocol, as pure functions.
//!
//! The mirror of [`super::agwinterm_wire`], and gated the same way — compiled on
//! macOS and in a test build everywhere — so the argv building, the projection
//! and the version rule are exercised by the Windows CI leg too, where the
//! adapter that calls them does not exist.
//!
//! Measured against agterm 0.34.0 (commit fe1948b9) rather than read off the
//! source, because three details of it are not derivable from the Swift:
//!
//! * **The `--` terminator must come after every option.** A context beginning
//!   with `-` is otherwise parsed as an unknown flag and the command fails with
//!   `provide exactly one of a TEXT or --clear`. `session context -- "-x"
//!   --target T` does not fix it either — everything after `--` becomes
//!   positional, so `--target` is then reported as an unexpected argument. Only
//!   options-then-`--`-then-text works, which is why [`context_argv`] builds the
//!   text last and the ordering is pinned by a test.
//! * **The budget is 256 UTF-8 bytes and is enforced**, not advisory: an
//!   over-long value is refused with `context must be at most 256 UTF-8 bytes`
//!   and the previous context stays. See [`CONTEXT_MAX_UTF8`].
//! * **An empty text is a refusal, never a clear** (`context must not be empty
//!   (use --clear to remove it)`), so [`LabelWrite::ClearContext`] maps to
//!   `--clear` and never to `""`.

#![cfg(any(target_os = "macos", test))]

use super::{LabelBudget, LabelTarget};

/// The longest context agterm accepts, in UTF-8 bytes (`Session.contextByteLimit`).
///
/// Enforced on the *trimmed* value, which is what makes
/// [`super::labels::fit_utf8`]'s `trim_end` necessary rather than tidy: agterm
/// stores the trimmed string, so a value whose cut left a trailing space would
/// never equal its own read-back and would be rewritten on every pass.
pub(crate) const CONTEXT_MAX_UTF8: usize = 256;

/// The first agterm release with a `session context` verb.
///
/// 0.25.0 — which this machine ran until the upgrade — has no such verb at all,
/// and answers `unexpected arguments: 'context'`. `can_label` gates on this
/// rather than probing the verb, so the refusal never reaches a log line.
pub(crate) const CONTEXT_SINCE: (u32, u32, u32) = (0, 26, 0);

/// The `version --json` answer's app version, or `None` when the shape is not
/// what this knows.
pub(crate) fn app_version(value: &serde_json::Value) -> Option<&str> {
    value.get("result")?.get("app")?.get("version")?.as_str()
}

/// Whether `version` is at least [`CONTEXT_SINCE`].
///
/// A plain major/minor/patch compare, lenient about anything after the patch (a
/// `-rc1` or a build suffix) and refusing anything it cannot parse — an
/// unreadable version is treated as too old, so the feature stays off rather
/// than writing into a terminal that may not understand it.
pub(crate) fn supports_context(version: &str) -> bool {
    let mut parts = version.split(['.', '-', '+']).map(str::parse::<u32>);
    let (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    (major, minor, patch) >= CONTEXT_SINCE
}

/// Every labellable session in one window's `tree --json`, as [`LabelTarget`]s.
///
/// The key is `<window>/<session>`. agterm resolves a session id across windows
/// on a write, so `--window` is not needed to address one — but a read is
/// per-window (a bare `tree` projects only the frontmost), so the window is
/// already in hand and carrying it costs nothing and keeps the key unique
/// without assuming anything about agterm's id space.
///
/// `context` is the tree's **shown** value. On a session attached from another
/// Mac that is the local override else the origin's mirrored context, so it can
/// be a value this dashboard never wrote — which cannot mislead the planner,
/// because an attached session's title carries the `⇄` badge and
/// `terminal_title::title_names` refuses a badged title, so no row ever claims
/// one.
pub(crate) fn targets_from(window: &str, sessions: &[serde_json::Value]) -> Vec<LabelTarget> {
    sessions
        .iter()
        .filter_map(|s| {
            let id = s.get("id").and_then(serde_json::Value::as_str)?;
            Some(LabelTarget {
                key: format!("{window}/{id}"),
                title: s.get("title").and_then(serde_json::Value::as_str).map(str::to_string),
                context: s.get("context").and_then(serde_json::Value::as_str).map(str::to_string),
                budget: LabelBudget::Utf8Bytes(CONTEXT_MAX_UTF8),
            })
        })
        .collect()
}

/// The window and session halves of a key [`targets_from`] minted.
///
/// agterm's ids are UUIDs, so the first `/` is unambiguous.
pub(crate) fn split_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('/')
}

/// The argv that sets `text` as `session`'s context in `window`.
///
/// `--` goes last, immediately before the text, for the reason the module doc
/// gives. Every element is owned because the text is.
pub(crate) fn context_argv(window: &str, session: &str, text: &str) -> Vec<String> {
    vec![
        "session".into(),
        "context".into(),
        "--target".into(),
        session.into(),
        "--window".into(),
        window.into(),
        "--json".into(),
        "--".into(),
        text.into(),
    ]
}

/// The argv that clears `session`'s context in `window`.
pub(crate) fn clear_argv(window: &str, session: &str) -> Vec<String> {
    vec!["session".into(), "context".into(), "--clear".into(), "--target".into(), session.into(), "--window".into(), window.into(), "--json".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_is_the_last_argument_after_a_terminator() {
        let argv = context_argv("W1", "S1", "-leading-dash task");
        assert_eq!(argv.last().unwrap(), "-leading-dash task");
        let terminator = argv.iter().position(|a| a == "--").expect("a -- terminator");
        assert_eq!(terminator, argv.len() - 2, "-- must sit immediately before the text");
        // Measured: a terminator before the options makes --target positional and
        // agtermctl reports it as an unexpected argument.
        assert!(argv.iter().position(|a| a == "--target").unwrap() < terminator);
    }

    #[test]
    fn a_clear_never_sends_an_empty_text() {
        let argv = clear_argv("W1", "S1");
        assert!(argv.iter().any(|a| a == "--clear"));
        assert!(!argv.iter().any(std::string::String::is_empty), "an empty text is refused, not a second way to clear");
        assert!(!argv.iter().any(|a| a == "--"), "nothing positional follows a clear");
    }

    #[test]
    fn targets_carry_the_byte_budget_and_the_window_scoped_key() {
        let sessions = vec![
            serde_json::json!({"id": "A57195F5", "title": "🔵 agterm", "context": "fix the gate"}),
            serde_json::json!({"id": "55DD5E08", "title": "⚫ ai-dashboard"}),
        ];
        let t = targets_from("3A0E9E5A", &sessions);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].key, "3A0E9E5A/A57195F5");
        assert_eq!(t[0].context.as_deref(), Some("fix the gate"));
        assert_eq!(t[0].budget, LabelBudget::Utf8Bytes(256));
        // An unset context is absent from the node, not empty.
        assert_eq!(t[1].context, None);
        assert_eq!(split_key(&t[0].key), Some(("3A0E9E5A", "A57195F5")));
    }

    #[test]
    fn a_session_with_no_id_is_skipped_rather_than_keyed_on_nothing() {
        let sessions = vec![serde_json::json!({"title": "🔵 dash"})];
        assert!(targets_from("W1", &sessions).is_empty());
    }

    #[test]
    fn the_version_gate_is_the_release_that_shipped_the_verb() {
        assert!(!supports_context("0.25.0"), "0.25.0 has no session context verb");
        assert!(supports_context("0.26.0"));
        assert!(supports_context("0.34.0"));
        assert!(supports_context("1.0.0"));
        assert!(supports_context("0.26.0-rc1"), "a suffix after the patch is ignored");
        assert!(!supports_context("0.9"), "an unparseable version is treated as too old");
        assert!(!supports_context("unknown"));
    }

    #[test]
    fn the_app_version_is_read_off_the_version_envelope() {
        let v = serde_json::json!({"ok": true, "result": {"app": {"version": "0.34.0", "commit": "fe1948b9"}}});
        assert_eq!(app_version(&v), Some("0.34.0"));
        assert_eq!(app_version(&serde_json::json!({"ok": true, "result": {}})), None);
    }
}
