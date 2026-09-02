//! Per-session attention: which finished sessions the user has actually looked
//! at, and which are still waiting to be read.
//!
//! The dashboard already knows *what* every agent is doing. It does not know
//! *whether you have looked* — and that fact never enters the process anywhere
//! else. `idle::idle_ms` reports input across the whole desktop, so it reads "at
//! the desk, don't bother him" while the user is typing in a different session
//! entirely, and it reads "away" the moment he steps out after reading a result.
//! Neither answer is about the row being judged. That is why
//! `notifications::fire_reason`'s AFK window cannot express this, at any value.
//!
//! The verdict lives on [`crate::state::AgentSession::attention`] and is turned
//! into the row's displayed status by `commands::apply_read_as_idle`. This module
//! is the *sensor*: every way the app learns the user looked at something. All of
//! them are **positive observations** — nothing here ever infers attention from an
//! absence, so a failure leaves a row *showing* rather than hiding it, and the
//! next observation corrects it.
//!
//! Two sources, and they are deliberately different shapes:
//!
//! - **The history window**, on every platform. `observe` is called when a row's
//!   history opens and again when it closes, which turns a click into a dwell so a
//!   read spanning a mid-read transcript flush still ends attended.
//! - **A terminal**, via [`crate::terminals::TerminalAdapter`]. Terminals differ
//!   in what they expose, so the terminal-specific half lives behind that trait —
//!   agterm today, a Windows terminal later — and this module never names one.
//!   What arrives here is a [`crate::terminals::Observation`]: a session named by
//!   `cwd` and `title`, an absolute instant, and whether the user *left* it or
//!   typed in it.
//!
//! Deliberately **not** sources: window focus, `toggle_main` / `reveal`, the
//! frontend's `visibilitychange`, and row hover. The first proves a window is
//! frontmost rather than that a human is present — the history window opens
//! maximized and is hidden rather than closed, so "left it up and walked away" is
//! its resting state — and the rest are widget-global, which is the exact axis
//! this feature exists to replace.
//!
//! Uncovered and accepted: read a finished session, then neither type nor switch
//! away. Nothing observes that under any design considered, and it fails in the
//! safe direction.

use crate::state::{AgentSession, AppState, Attention};
use crate::terminals::{Observation, ObservationKind, TerminalSession};
use tauri::{AppHandle, Manager};

/// How often the sensor wakes. It asks the terminal nothing unless
/// [`should_poll`] says to, so this is a decision cadence, not a subprocess rate.
///
/// It does **not** need to be fast, and that is a property of the design rather
/// than a tolerance: every [`Observation`] carries an absolute instant, so a
/// reading taken late reports the same instant as one taken immediately. The
/// interval governs only how soon the pill catches up on screen.
const TICK_MS: u64 = 30_000;

/// How the app came to believe the user looked at a session. Logged as the
/// `source` field on `decision = "attention_seen"`, so the reason a row went
/// quiet is answerable from `widget.jsonl` alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionSource {
    HistoryOpened,
    HistoryClosed,
    /// The user left the session's tab, having been in it — the terminal's
    /// primary signal.
    TerminalDeparted,
    /// The user produced input while the session was the one on screen.
    TerminalInput,
}

impl AttentionSource {
    fn key(self) -> &'static str {
        match self {
            Self::HistoryOpened => "history_opened",
            Self::HistoryClosed => "history_closed",
            Self::TerminalDeparted => "terminal_departed",
            Self::TerminalInput => "terminal_input",
        }
    }
}

impl From<ObservationKind> for AttentionSource {
    fn from(kind: ObservationKind) -> Self {
        match kind {
            ObservationKind::Departed => Self::TerminalDeparted,
            ObservationKind::Input => Self::TerminalInput,
        }
    }
}

/// Whether this tick should spend anything asking the terminal.
///
/// Pure, so the cost is pinned by a test rather than measured in production. The
/// steady state — every finished row already read, or nothing finished at all —
/// costs nothing, which is the case the machine is in most of the day.
///
/// There is deliberately **no back-off** for a row unread a long while. One
/// existed and caused a real miss: the rationale for backing off, "the answer
/// stops changing quickly", is true of a row's *status* and false of a *visit*,
/// which can happen at any instant. A long-unread row is if anything the likeliest
/// one to be opened next.
pub fn should_poll(sessions: &[AgentSession]) -> bool {
    sessions.iter().any(|s| s.origin.is_none() && s.attention() == Attention::Pending)
}

/// Which local row a terminal session is, given how its terminal names it.
///
/// The title is preferred over the working directory, and that ordering is the
/// point. A row's id is cwd-derived only as a *first-seen anchor*:
/// `ChatIdRegistry` pins it thereafter, so a session that has `cd`-ed into a
/// subdirectory reports a cwd deriving some *other* row's id — and stamping that
/// row would mark work read that nobody has looked at, which is the one direction
/// of error this feature must not make. The title is the string this dashboard
/// itself wrote onto that tab (`terminal_title::build_title`, "<glyph> <name>"),
/// so it names the row rather than guessing at it.
///
/// Falls back to the cwd join when no title is recognizable — `terminal_titles`
/// can be off, and a session can predate the dashboard writing to it — and
/// answers `None` rather than guessing when neither resolves.
pub fn resolve_row(session: &TerminalSession, sessions: &[AgentSession], projects_root: Option<&str>) -> Option<String> {
    if let Some(title) = session.title.as_deref() {
        // "<glyph> <name>" with an optional " [N%]" / " ⚠" suffix. Match on the
        // name rather than reconstructing the whole title, which would have to
        // track every suffix `build_title` learns later.
        let body = title.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
        let named = sessions.iter().filter(|s| s.origin.is_none()).find(|s| {
            let label = s.display_label();
            body == label || body.starts_with(&format!("{label} "))
        });
        if let Some(s) = named {
            return Some(s.id.clone());
        }
    }
    let cwd = session.cwd.as_deref()?;
    let derived = crate::adapters::claude::derive_chat_id(Some(cwd), projects_root);
    sessions.iter().find(|s| s.origin.is_none() && s.id == derived).map(|s| s.id.clone())
}

/// Start the sensor: a tick that asks this platform's terminal adapter what the
/// user has been doing.
///
/// A no-op where no adapter exists — see [`crate::terminals::for_platform`]. It
/// runs on a blocking thread rather than the async runtime because an adapter may
/// spawn a subprocess under a kill timeout, and it is deliberately *not* hung off
/// `commands::emit_sessions_updated`, which runs synchronously inside the axum
/// hook handler and inside the watcher thread and has no business waiting on a
/// terminal.
pub fn spawn(app: AppHandle) {
    let Some(mut adapter) = crate::terminals::for_platform() else { return };
    let terminal = adapter.name();

    // The push half. A terminal that can be watched reports a departure the
    // moment it happens rather than at the next tick — which matters because the
    // tick is *discovery lag*, not a chosen delay: while polling, the app simply
    // does not know the user left until it next asks.
    let (tx, rx) = std::sync::mpsc::channel();
    adapter.watch(tx);
    let watched = app.clone();
    std::thread::spawn(move || {
        while let Ok(observation) = rx.recv() {
            let Some(state) = watched.try_state::<AppState>() else { continue };
            let projects_root = watched.try_state::<crate::config::ConfigState>().and_then(|c| c.config.lock().unwrap().projects_root.clone());
            apply(&watched, &state.snapshot(), terminal, &observation, projects_root.as_deref());
        }
    });

    // The pull half, kept as the safety net rather than the primary source: it
    // carries `idleMs` (which no snapshot file has), it enumerates windows, and
    // it keeps working when the watch cannot start — a schema change, a missing
    // directory, a permission problem.
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(TICK_MS));
        tick(&app, adapter.as_mut());
    });
}

/// One sensor pass: ask the adapter, resolve each observation to a row, stamp it.
fn tick(app: &AppHandle, adapter: &mut dyn crate::terminals::TerminalAdapter) {
    if !app.try_state::<crate::config::ConfigState>().is_some_and(|c| c.config.lock().unwrap().attention_tracking) {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else { return };
    let sessions = state.snapshot();
    if !should_poll(&sessions) {
        return;
    }
    let now = crate::commands::now_ms();
    let projects_root = app.try_state::<crate::config::ConfigState>().and_then(|c| c.config.lock().unwrap().projects_root.clone());
    for observation in adapter.poll(now) {
        apply(app, &sessions, adapter.name(), &observation, projects_root.as_deref());
    }
}

/// Resolve one observation and stamp the row it names.
///
/// Split out from [`tick`] and taking the sessions it judges against, so the
/// resolution half is testable without a terminal or an `AppHandle`.
fn apply(app: &AppHandle, sessions: &[AgentSession], terminal: &'static str, observation: &Observation, projects_root: Option<&str>) {
    let Some(id) = resolve_row(&observation.session, sessions, projects_root) else {
        tracing::debug!(
            decision = "attention_poll",
            terminal,
            outcome = "no_target",
            kind = ?observation.kind,
            title = ?observation.session.title,
            "the terminal named a session matching no row"
        );
        return;
    };
    observe(app, &id, observation.at_ms, observation.kind.into());
}

/// Record an observation that the user attended to `id` at `at_ms`, and push the
/// change to the UI if it changed the row's verdict.
///
/// The single entry point for every sensor, so the stamp, the decision log and
/// the emit can't drift apart between call sites.
///
/// Gated on `config.attention_tracking`: with the feature off nothing is ever
/// stamped, so turning it on later starts from a clean slate rather than
/// resurrecting observations made while it was disabled.
///
/// Returns whether this changed the row's verdict — false covers a row that was
/// already read, one that isn't asking to be, and one that no longer exists.
pub fn observe(app: &AppHandle, id: &str, at_ms: i64, source: AttentionSource) -> bool {
    if !app.try_state::<crate::config::ConfigState>().is_some_and(|c| c.config.lock().unwrap().attention_tracking) {
        return false;
    }
    let Some(state) = app.try_state::<AppState>() else { return false };
    if !state.mark_attended(id, at_ms) {
        return false;
    }
    tracing::debug!(id = %id, decision = "attention_seen", source = source.key(), attended_at = at_ms, "session marked as read");
    crate::commands::emit_sessions_updated(app);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Status;

    fn row(id: &str, status: Status, state_entered_at: i64, attended_at: Option<i64>) -> AgentSession {
        let state = AppState::new();
        state.apply_set(
            crate::state::SetInput {
                id: id.into(),
                status,
                label: None,
                source: None,
                model: None,
                input_tokens: None,
                dialog_entry: None,
                waiting_backstop_armed: false,
            },
            state_entered_at,
            &[],
            None,
        );
        let mut s = state.snapshot().pop().expect("row");
        s.attended_at = attended_at;
        s
    }

    fn named(title: Option<&str>, cwd: Option<&str>) -> TerminalSession {
        TerminalSession { title: title.map(str::to_string), cwd: cwd.map(str::to_string) }
    }

    #[test]
    fn the_title_names_the_row_and_outranks_the_working_directory() {
        // The hazard this ordering exists for: a session that `cd`-ed into a
        // subdirectory reports a cwd deriving *another* row's id, and stamping
        // that row would mark unread work as read.
        let sessions = vec![row("dash", Status::Done, 0, None), row("sub", Status::Done, 0, None)];
        let s = named(Some("🟢 dash"), Some("/p/dash/sub"));
        assert_eq!(resolve_row(&s, &sessions, None).as_deref(), Some("dash"), "the title wins over the misleading cwd");
    }

    #[test]
    fn a_title_with_suffixes_still_names_its_row() {
        // `build_title` appends " [N%]" and " ⚠"; matching the whole string would
        // have to learn every suffix it grows later.
        let sessions = vec![row("dash", Status::Done, 0, None)];
        for title in ["🟢 dash", "🟢 dash [62%]", "🟢 dash ⚠", "🟢 dash [62%] ⚠"] {
            assert_eq!(resolve_row(&named(Some(title), None), &sessions, None).as_deref(), Some("dash"), "{title}");
        }
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_not_confused_for_it() {
        let sessions = vec![row("dash", Status::Done, 0, None), row("dashboard", Status::Done, 0, None)];
        assert_eq!(resolve_row(&named(Some("🟢 dashboard"), None), &sessions, None).as_deref(), Some("dashboard"));
        assert_eq!(resolve_row(&named(Some("🟢 dash"), None), &sessions, None).as_deref(), Some("dash"));
    }

    #[test]
    fn without_a_usable_title_it_falls_back_to_the_cwd_join() {
        // `terminal_titles` can be off, and a session can predate the dashboard
        // ever writing to that tab.
        let sessions = vec![row("dash", Status::Done, 0, None)];
        assert_eq!(resolve_row(&named(None, Some("/p/dash")), &sessions, None).as_deref(), Some("dash"));
    }

    #[test]
    fn an_unmatchable_session_resolves_to_nothing() {
        let sessions = vec![row("dash", Status::Done, 0, None)];
        assert_eq!(resolve_row(&named(Some("🟢 stranger"), Some("/p/elsewhere")), &sessions, None), None, "no row is better than the wrong row");
    }

    #[test]
    fn a_remote_row_is_never_resolved() {
        // Attention is about the human at this keyboard; another machine's rows
        // are not ours to mark read.
        let mut remote = row("dash", Status::Done, 0, None);
        remote.origin = Some("chrome".into());
        assert_eq!(resolve_row(&named(Some("🟢 dash"), Some("/p/dash")), &[remote], None), None);
    }

    #[test]
    fn the_sensor_asks_nothing_when_every_finished_row_is_read() {
        // The steady state, and the case the machine is in most of the time.
        assert!(!should_poll(&[]), "no rows at all");
        assert!(!should_poll(&[row("a", Status::Working, 0, None)]), "nothing finished");
        assert!(!should_poll(&[row("a", Status::Done, 0, Some(999_999))]), "finished and already read");
    }

    #[test]
    fn an_unread_row_is_watched_however_old_it_is() {
        // A back-off after two minutes used to live here and caused a real miss: a
        // tab opened and left inside one interval was never observed.
        assert!(should_poll(&[row("a", Status::Done, 0, None)]), "unread an hour ago");
        assert!(should_poll(&[row("a", Status::Done, 1_000_000, None)]), "unread just now");
    }

    #[test]
    fn a_remote_unread_row_never_makes_this_machine_ask() {
        let mut remote = row("a", Status::Done, 1_000_000, None);
        remote.origin = Some("chrome".into());
        assert!(!should_poll(&[remote]));
    }

    #[test]
    fn every_observation_kind_maps_to_a_distinct_logged_source() {
        assert_eq!(AttentionSource::from(ObservationKind::Departed).key(), "terminal_departed");
        assert_eq!(AttentionSource::from(ObservationKind::Input).key(), "terminal_input");
    }
}
