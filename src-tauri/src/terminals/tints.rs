//! Tinting each session's sidebar row: purple (by default) while a session runs
//! a model other than the default, and a `⇄` tab's own colour otherwise.
//!
//! **This dashboard owns the tint on every agent tab and every `⇄` tab** on the
//! machine it runs on, from its own `config.tab_tints`, so two machines can
//! colour the same model differently and neither reaches the other's terminal.
//! A `⇄` tab rendering another machine's session is tinted here from the
//! synced row's model, which that machine's dashboard learned from the hooks
//! and pushed.
//!
//! The rule, per target, first match wins:
//!
//! 1. its row runs a model off the default, and `tab_tints.model` is set: that
//! 2. it is a `⇄` tab ([`LabelTarget::attached_remote`]): `tab_tints.remote`
//! 3. a row claims it: cleared, except that a row whose model is not known yet
//!    leaves a model tint standing, since unknown is not the default: a row
//!    `session_restore` brings back, or one a `/clear` recreates, learns its
//!    model only from its next hook event or reply
//! 4. it carries one of this dashboard's titles naming no row, or still shows
//!    the tint this process wrote for a row that has since left: cleared, by
//!    the rule `labels` withdraws a context by (`Claim::ours_to_clear`)
//! 5. anything else is not ours and is left as it is
//!
//! A row `labels::join` declines (two tabs carrying its title, the shared-row
//! fault, or a draw between identical labels) claims no tab here either, so
//! its tabs fall to rule 4 and lose a model tint for as long as the overlap
//! lasts; a `⇄` tab among them keeps the remote tint.
//!
//! A `⇄` tab is recognised by what runs in it, which agterm reports from the
//! tab's first moment, while its row is still found through the badged title
//! `labels::join` reads, so a fresh `⇄` tab takes the remote tint at once and
//! a model tint once its title names the row.
//!
//! **The default model** is `env.ANTHROPIC_MODEL` in Claude Code's user
//! settings, read on every pass, and a model counts as the default when its id
//! contains that alias (`opus` in `claude-opus-5-5`). Where the settings
//! cannot be read the pass writes nothing, since without the default no target
//! can be decided and clearing would wipe tints a readable file would keep.
//!
//! Runs inside `labels`' worker, on the targets that pass already read, so it
//! shares its timing and its join; a failed tint write ends the tint half of
//! the pass and never the labels'.

use std::collections::HashMap;

use super::labels::{join, Claim, DesiredLabel, Declined};
use super::{LabelTarget, TerminalAdapter, Tint};
use crate::config::TabTints;

/// What the default model is, as Claude Code's user settings pin it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultModel {
    /// `env.ANTHROPIC_MODEL`, trimmed and lowercased, its `[1m]`-style suffix
    /// dropped.
    Alias(String),
    /// The settings name no default, so no model is off it.
    Unpinned,
}

/// Read the default model from `settings.json` under Claude Code's config
/// directory. `Err` says why it could not be read.
pub fn default_model() -> Result<DefaultModel, String> {
    let path = crate::token_scan::config_dir().ok_or("no home directory to find Claude Code's settings in")?.join("settings.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(default_from(&value))
}

/// The default model a parsed `settings.json` pins.
pub fn default_from(settings: &serde_json::Value) -> DefaultModel {
    let alias = settings.get("env").and_then(|e| e.get("ANTHROPIC_MODEL")).and_then(serde_json::Value::as_str).map(|m| m.split('[').next().unwrap_or("").trim().to_lowercase()).filter(|m| !m.is_empty());
    alias.map_or(DefaultModel::Unpinned, DefaultModel::Alias)
}

/// Whether `model` is off the default: known, and not containing the alias.
pub fn off_default(model: Option<&str>, default: &DefaultModel) -> bool {
    match (model, default) {
        (Some(m), DefaultModel::Alias(alias)) => !m.to_lowercase().contains(alias.as_str()),
        _ => false,
    }
}

/// Why a tint is written, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Model,
    Remote,
    Clear,
    Withdraw,
}

impl Why {
    pub fn slug(self) -> &'static str {
        match self {
            Why::Model => "model",
            Why::Remote => "remote",
            Why::Clear => "clear",
            Why::Withdraw => "withdraw",
        }
    }
}

/// One tint write the plan calls for. `tint` is `None` for a clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TintWrite {
    pub key: String,
    pub tint: Option<Tint>,
    pub replaced: Option<Tint>,
    /// The row the target is joined to, where one is.
    pub row: Option<String>,
    pub why: Why,
}

/// The tint writes that bring every target in line with the module doc's
/// rule. Pure. `written` maps each key this process last tinted to the row it
/// tinted it for and the tint it wrote.
pub fn plan(rows: &[DesiredLabel], targets: &[LabelTarget], tints: &TabTints, default: &DefaultModel, written: &HashMap<String, (String, Tint)>) -> Vec<TintWrite> {
    let titles: Vec<Option<&str>> = targets.iter().map(|t| t.title.as_deref()).collect();
    let mut declined: Vec<Declined> = Vec::new();
    let claims = join(rows, &titles, &mut declined);
    let mut out = Vec::new();
    for (t, claim) in targets.iter().zip(&claims) {
        let row = match claim {
            Claim::Row(r) => Some(*r),
            _ => None,
        };
        let model_tint = tints.model.filter(|_| row.is_some_and(|r| off_default(r.model.as_deref(), default)));
        if row.is_some_and(|r| r.model.is_none()) && t.tint.is_some() && t.tint == tints.model {
            continue;
        }
        let (want, why) = if let Some(m) = model_tint {
            (Some(m), Why::Model)
        } else if t.attached_remote {
            (tints.remote, Why::Remote)
        } else if row.is_some() {
            (None, Why::Clear)
        } else {
            if !claim.ours_to_clear(written.get(&t.key).map(|(r, tint)| (r.as_str(), tint)), t.tint.as_ref(), rows) {
                continue;
            }
            (None, Why::Withdraw)
        };
        if t.tint != want {
            out.push(TintWrite { key: t.key.clone(), tint: want, replaced: t.tint, row: row.map(|r| r.row.clone()), why });
        }
    }
    out
}

/// Whether the tint half of a pass wrote, found nothing to do, or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Unchanged,
    /// The default model could not be read; nothing was written.
    NoDefault,
    WriteFailed,
}

impl Outcome {
    pub fn slug(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::Unchanged => "unchanged",
            Outcome::NoDefault => "no_default",
            Outcome::WriteFailed => "write_failed",
        }
    }
}

/// Plan against `targets` and make the writes, keeping `written` current.
pub fn pass(adapter: &dyn TerminalAdapter, rows: &[DesiredLabel], targets: &[LabelTarget], tints: &TabTints, written: &mut HashMap<String, (String, Tint)>) -> Outcome {
    let default = match default_model() {
        Ok(d) => d,
        Err(error) => {
            tracing::debug!(error = %error, "could not read the default model; no tint is written this pass");
            return Outcome::NoDefault;
        }
    };
    let mut wrote = false;
    for w in plan(rows, targets, tints, &default, written) {
        let result = adapter.write_tint(&w.key, w.tint.as_ref());
        let value = w.tint.map(|t| format!("{t:?}")).unwrap_or_default();
        let replaced = w.replaced.map(|t| format!("{t:?}")).unwrap_or_default();
        let row = w.row.as_deref().unwrap_or("");
        match &result {
            Ok(()) => tracing::info!(decision = "tint_write", chat_id = %row, key = %w.key, why = w.why.slug(), value, replaced, ok = true, "terminal session tint written"),
            Err(error) => tracing::warn!(decision = "tint_write", chat_id = %row, key = %w.key, why = w.why.slug(), value, replaced, ok = false, error = %error, "terminal session tint write failed"),
        }
        if result.is_err() {
            return Outcome::WriteFailed;
        }
        wrote = true;
        match (w.tint, &w.row) {
            (Some(tint), Some(row)) => {
                written.insert(w.key.clone(), (row.clone(), tint));
            }
            _ => {
                written.remove(&w.key);
            }
        }
    }
    if wrote {
        Outcome::Applied
    } else {
        Outcome::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{LabelBudget, TintColour, TintOn};

    const PURPLE: Tint = Tint { colour: TintColour::Purple, on: TintOn::Name };
    const GRAY: Tint = Tint { colour: TintColour::Gray, on: TintOn::IconAndName };

    fn opus() -> DefaultModel {
        DefaultModel::Alias("opus".into())
    }

    fn tints() -> TabTints {
        TabTints { model: Some(PURPLE), remote: Some(GRAY) }
    }

    fn row(id: &str, model: Option<&str>) -> DesiredLabel {
        DesiredLabel { row: id.into(), label: id.into(), context: None, remote: false, model: model.map(Into::into) }
    }

    fn synced(name: &str, model: Option<&str>) -> DesiredLabel {
        DesiredLabel { row: format!("CHROME/{name}"), remote: true, ..row(name, model) }
    }

    fn target(key: &str, title: Option<&str>, tint: Option<Tint>, attached_remote: bool) -> LabelTarget {
        LabelTarget { key: key.into(), title: title.map(Into::into), context: None, budget: LabelBudget::Utf8Bytes(256), tint, attached_remote }
    }

    fn whys(writes: &[TintWrite]) -> Vec<(&str, Option<Tint>, Why)> {
        writes.iter().map(|w| (w.key.as_str(), w.tint, w.why)).collect()
    }

    #[test]
    fn the_default_is_the_settings_alias_without_its_window_suffix() {
        assert_eq!(default_from(&serde_json::json!({"env": {"ANTHROPIC_MODEL": " Opus[1m] "}})), DefaultModel::Alias("opus".into()));
        assert_eq!(default_from(&serde_json::json!({"env": {}})), DefaultModel::Unpinned);
        assert_eq!(default_from(&serde_json::json!({})), DefaultModel::Unpinned);
    }

    #[test]
    fn a_model_is_off_the_default_only_when_known_and_not_containing_the_alias() {
        assert!(!off_default(Some("claude-opus-5-5"), &opus()));
        assert!(off_default(Some("claude-fable-5-2"), &opus()));
        assert!(!off_default(None, &opus()));
        assert!(!off_default(Some("claude-fable-5-2"), &DefaultModel::Unpinned));
    }

    #[test]
    fn a_local_session_off_the_default_is_tinted_and_cleared_on_return() {
        let off = plan(&[row("dash", Some("claude-fable-5-2"))], &[target("k", Some("🔵 dash"), None, false)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&off), vec![("k", Some(PURPLE), Why::Model)]);
        let back = plan(&[row("dash", Some("claude-opus-5-5"))], &[target("k", Some("🔵 dash"), Some(PURPLE), false)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&back), vec![("k", None, Why::Clear)]);
    }

    #[test]
    fn a_remote_tab_takes_the_model_tint_over_the_remote_one_and_gets_it_back() {
        let rows = [synced("dash", Some("claude-fable-5-2"))];
        let off = plan(&rows, &[target("k", Some("⇄ 🔵 dash"), Some(GRAY), true)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&off), vec![("k", Some(PURPLE), Why::Model)]);
        let rows = [synced("dash", Some("claude-opus-5-5"))];
        let back = plan(&rows, &[target("k", Some("⇄ 🔵 dash"), Some(PURPLE), true)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&back), vec![("k", Some(GRAY), Why::Remote)]);
    }

    #[test]
    fn a_fresh_remote_tab_takes_the_remote_tint_before_its_title_names_a_row() {
        let writes = plan(&[], &[target("k", Some("⇄ dash · connecting"), None, true)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&writes), vec![("k", Some(GRAY), Why::Remote)]);
    }

    #[test]
    fn a_tab_already_showing_what_it_wants_is_not_written() {
        let writes = plan(&[row("dash", Some("claude-fable-5-2"))], &[target("k", Some("🔵 dash"), Some(PURPLE), false)], &tints(), &opus(), &HashMap::new());
        assert!(writes.is_empty());
    }

    #[test]
    fn a_foreign_tab_keeps_its_tint_unless_this_process_placed_it_for_a_row_that_left() {
        let foreign = target("k", Some("zsh"), Some(PURPLE), false);
        assert!(plan(&[], std::slice::from_ref(&foreign), &tints(), &opus(), &HashMap::new()).is_empty());
        let written = HashMap::from([("k".to_string(), ("dash".to_string(), PURPLE))]);
        assert_eq!(whys(&plan(&[], &[foreign], &tints(), &opus(), &written)), vec![("k", None, Why::Withdraw)]);
        let retinted = target("k", Some("zsh"), Some(GRAY), false);
        assert!(plan(&[], &[retinted], &tints(), &opus(), &written).is_empty(), "a tint set since this process wrote its own is not withdrawn");
    }

    #[test]
    fn a_row_whose_model_is_not_known_yet_keeps_a_model_tint() {
        let restored = plan(&[row("dash", None)], &[target("k", Some("🟢 dash"), Some(PURPLE), false)], &tints(), &opus(), &HashMap::new());
        assert!(restored.is_empty());
        let remote = plan(&[synced("dash", None)], &[target("k", Some("⇄ 🟢 dash"), Some(PURPLE), true)], &tints(), &opus(), &HashMap::new());
        assert!(remote.is_empty());
        let untinted = plan(&[synced("dash", None)], &[target("k", Some("⇄ 🟢 dash"), None, true)], &tints(), &opus(), &HashMap::new());
        assert_eq!(whys(&untinted), vec![("k", Some(GRAY), Why::Remote)], "an unknown model still takes the remote tint where no model tint stands");
    }

    #[test]
    fn an_unset_model_tint_leaves_a_session_to_the_remaining_rules() {
        let none = TabTints { model: None, remote: Some(GRAY) };
        let writes = plan(&[synced("dash", Some("claude-fable-5-2"))], &[target("k", Some("⇄ 🔵 dash"), Some(PURPLE), true)], &none, &opus(), &HashMap::new());
        assert_eq!(whys(&writes), vec![("k", Some(GRAY), Why::Remote)]);
    }
}
