//! Ends a local session that has sat CLEAN for `config.idle_close_ms`: the agent
//! and the terminal tab it runs in.
//!
//! Only `Status::Idle` qualifies. It is the one status set on positive evidence
//! that nothing is left to come back to (a `/clear`, a fresh start, a `/pull`
//! that reported clean), so closing it loses nothing a person had not already
//! finished with; the conversation stays on disk and `claude --continue` in
//! that folder resumes it. A `Done` is never closed, read or not, nor anything a subagent's
//! dialog is holding, since that row reads `Blocked`.
//!
//! **The session is found by the label join**, `terminals::labels::join`, so the
//! tab this closes is the tab the context line is written to, and the two cannot
//! disagree about which session is a row's. Three more conditions guard the
//! close: the tab's title, as the terminal reports it, must carry the `Idle`
//! glyph, so a tab still showing another status is left alone; the row is
//! re-read from this process's state just before the close, so a prompt that
//! arrived while the terminal was being asked cancels it; and the adapter may
//! refuse on a fresh reading of its own, as agterm does when the pane the title
//! belongs to is no longer running the agent.
//!
//! What closing takes is the adapter's ([`TerminalAdapter::close_session`]).
//! Removing the row is not done here: the agent exiting ends it through the
//! paths every exit takes, a `SessionEnd` or the liveness reaper.
//!
//! A tab mirroring another machine's session (the `⇄` tabs) is never closed
//! from this side, because the join refuses a remote title. When the machine
//! that owns the session ends it, the tab's `attach.sh` exits and closes its
//! own tab.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::ConfigState;
use crate::state::{AppState, Status};
use crate::terminal_title::parse_title;
use crate::terminals::labels::{join, Claim, DesiredLabel};
use crate::terminals::{LabelTarget, TerminalAdapter};

/// How often rows are checked. The window is an hour by default, so a minute's
/// lateness is immaterial, and a tick with nothing due asks the terminal nothing.
const POLL: Duration = Duration::from_secs(60);

/// Whether a row has been CLEAN for at least `window_ms`. `0` disables.
fn is_due(status: Status, state_entered_at: i64, now_ms: i64, window_ms: u64) -> bool {
    window_ms > 0 && status == Status::Idle && now_ms.saturating_sub(state_entered_at) >= window_ms as i64
}

/// What becomes of one due row this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    /// Its one session, by key, which shows the `Idle` glyph.
    Close(String),
    /// No session's title names it.
    NoSession,
    /// Its session's title names it but shows another status, so the tab is not
    /// following the row and is left alone.
    TabNotIdle,
    /// The join refused it: `same_label` or `shared_title`.
    Declined(&'static str),
}

impl Verdict {
    fn slug(&self) -> &'static str {
        match self {
            Verdict::Close(_) => "close",
            Verdict::NoSession => "no_session",
            Verdict::TabNotIdle => "tab_not_idle",
            Verdict::Declined(reason) => reason,
        }
    }

    /// The log line's human `reason`.
    fn reason(&self) -> &'static str {
        match self {
            Verdict::Close(_) => "clean past the window; closed the session and its tab",
            Verdict::NoSession => "clean past the window, but no terminal session's title names this row",
            Verdict::TabNotIdle => "clean past the window, but its tab is showing another status, so it is not following the row",
            Verdict::Declined("same_label") => "clean past the window, but its tab's title names this row and another carrying the same label",
            Verdict::Declined(_) => "clean past the window, but more than one tab's title names this row",
        }
    }
}

/// The verdict for every row in `due`. Pure. `rows` is every local row, due or
/// not, because the join ranks a title against all of them: a title naming
/// `bga assistant` must go to that row and not to a due `bga`.
fn pick(rows: &[DesiredLabel], targets: &[LabelTarget], due: &HashSet<&str>) -> Vec<(String, Verdict)> {
    let mut declined = Vec::new();
    let claims = join(rows, targets, &mut declined);
    let mut out: Vec<(String, Verdict)> = Vec::new();
    for row in rows.iter().filter(|r| due.contains(r.row.as_str())) {
        let verdict = if let Some(d) = declined.iter().find(|d| d.row == row.row) {
            Verdict::Declined(d.reason)
        } else {
            match targets.iter().zip(&claims).find(|(_, c)| matches!(c, Claim::Row(r) if r.row == row.row)) {
                None => Verdict::NoSession,
                Some((t, _)) => match t.title.as_deref().and_then(parse_title) {
                    Some(reading) if reading.status == Status::Idle => Verdict::Close(t.key.clone()),
                    _ => Verdict::TabNotIdle,
                },
            }
        };
        out.push((row.row.clone(), verdict));
    }
    out
}

pub fn spawn(app: AppHandle) {
    let Some(adapter) = crate::terminals::for_platform(&app).filter(|a| a.can_close()) else { return };
    std::thread::spawn(move || {
        // The last verdict logged per row, so a row that stays unclosable is
        // reported once rather than every tick.
        let mut logged: HashMap<String, &'static str> = HashMap::new();
        loop {
            std::thread::sleep(POLL);
            tick(&app, adapter.as_ref(), &mut logged);
        }
    });
}

fn tick(app: &AppHandle, adapter: &dyn TerminalAdapter, logged: &mut HashMap<String, &'static str>) {
    let Some(cfg) = app.try_state::<ConfigState>() else { return };
    let window = cfg.snapshot().idle_close_ms.unwrap_or(0);
    let now = now_ms();
    let local: Vec<_> = crate::commands::resolved_snapshot(app).into_iter().filter(|s| s.origin.is_none()).collect();
    let due: HashSet<&str> = local.iter().filter(|s| is_due(s.status, s.state_entered_at, now, window)).map(|s| s.id.as_str()).collect();
    logged.retain(|row, _| due.contains(row.as_str()));
    if due.is_empty() {
        return;
    }
    let Some(targets) = adapter.label_targets() else {
        tracing::debug!(terminal = adapter.name(), "idle close: the terminal could not be asked; retrying next tick");
        return;
    };
    let rows: Vec<DesiredLabel> = local.iter().map(|s| DesiredLabel { row: s.id.clone(), label: s.display_label().to_string(), context: None }).collect();

    for (row, verdict) in pick(&rows, &targets, &due) {
        let Some(session) = local.iter().find(|s| s.id == row) else { continue };
        let Verdict::Close(key) = verdict else {
            // A closed session's row waits for `SessionEnd` or the reaper, and
            // its tab is already gone, which is not news.
            if logged.get(&row) == Some(&"closed") {
                continue;
            }
            if logged.insert(row.clone(), verdict.slug()) != Some(verdict.slug()) {
                tracing::info!(chat_id = %row, decision = "idle_close", outcome = verdict.slug(), terminal = adapter.name(), reason = verdict.reason(), "decision");
            }
            continue;
        };
        // A prompt that arrived while the terminal was being read moves the row
        // off `Idle` or restamps it; either cancels the close.
        let unchanged = app.try_state::<AppState>().and_then(|st| st.status_since(&row)) == Some((Status::Idle, session.state_entered_at));
        if !unchanged {
            continue;
        }
        let idle_ms = now.saturating_sub(session.state_entered_at);
        match adapter.close_session(&key) {
            Ok(()) => {
                logged.insert(row.clone(), "closed");
                tracing::info!(chat_id = %row, decision = "idle_close", outcome = "closed", terminal = adapter.name(), idle_ms, window_ms = window, reason = Verdict::Close(key).reason(), "decision");
            }
            Err(error) => {
                if logged.insert(row.clone(), "close_failed") != Some("close_failed") {
                    tracing::warn!(chat_id = %row, decision = "idle_close", outcome = "close_failed", terminal = adapter.name(), %error, "decision");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::LabelBudget;

    const WINDOW: u64 = 3_600_000;

    fn row(id: &str) -> DesiredLabel {
        DesiredLabel { row: id.into(), label: id.into(), context: None }
    }

    fn tab(key: &str, title: &str) -> LabelTarget {
        LabelTarget { key: key.into(), title: Some(title.into()), context: None, budget: LabelBudget::Utf8Bytes(256) }
    }

    /// A title as `terminal_title::sync` writes it for an `Idle` row.
    fn idle_title(label: &str) -> String {
        format!("⚫ {label}")
    }

    #[test]
    fn every_verdict_gives_its_own_reason() {
        let all = [Verdict::Close("k".into()), Verdict::NoSession, Verdict::TabNotIdle, Verdict::Declined("same_label"), Verdict::Declined("shared_title")];
        let reasons: HashSet<&str> = all.iter().map(Verdict::reason).collect();
        assert_eq!(reasons.len(), all.len());
    }

    #[test]
    fn only_idle_past_the_window_is_due() {
        assert!(is_due(Status::Idle, 0, WINDOW as i64, WINDOW));
        assert!(!is_due(Status::Idle, 0, WINDOW as i64 - 1, WINDOW));
        for s in [Status::Done, Status::Blocked, Status::Waiting, Status::Working, Status::Error] {
            assert!(!is_due(s, 0, i64::MAX, WINDOW), "{s:?} is never closed");
        }
        assert!(!is_due(Status::Idle, 0, i64::MAX, 0), "a zero window disables");
    }

    #[test]
    fn the_session_whose_idle_title_names_the_row_is_closed() {
        let rows = [row("web"), row("api")];
        let targets = [tab("w/1", &idle_title("web")), tab("w/2", &idle_title("api"))];
        let due: HashSet<&str> = ["web"].into();
        assert_eq!(pick(&rows, &targets, &due), vec![("web".to_string(), Verdict::Close("w/1".into()))]);
    }

    #[test]
    fn the_longest_label_wins_so_a_due_parent_does_not_take_its_childs_tab() {
        let rows = [row("bga"), row("bga assistant")];
        let targets = [tab("w/1", &idle_title("bga assistant"))];
        let due: HashSet<&str> = ["bga"].into();
        assert_eq!(pick(&rows, &targets, &due), vec![("bga".to_string(), Verdict::NoSession)]);
    }

    #[test]
    fn a_tab_showing_another_status_is_left_alone() {
        let rows = [row("web")];
        let due: HashSet<&str> = ["web"].into();
        assert_eq!(pick(&rows, &[tab("w/1", "🔵 web")], &due), vec![("web".to_string(), Verdict::TabNotIdle)]);
    }

    #[test]
    fn two_tabs_on_one_row_close_neither() {
        let rows = [row("web")];
        let targets = [tab("w/1", &idle_title("web")), tab("w/2", &idle_title("web"))];
        let due: HashSet<&str> = ["web"].into();
        assert_eq!(pick(&rows, &targets, &due), vec![("web".to_string(), Verdict::Declined("shared_title"))]);
    }

    #[test]
    fn a_title_this_dashboard_did_not_write_names_nothing() {
        let rows = [row("web")];
        let due: HashSet<&str> = ["web"].into();
        assert_eq!(pick(&rows, &[tab("w/1", "~/web — zsh")], &due), vec![("web".to_string(), Verdict::NoSession)]);
    }
}
