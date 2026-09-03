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

/// This machine's rows, with their display names resolved.
///
/// The resolution is not a nicety here, it is the difference between the sensor
/// working and doing nothing at all for a renamed row. [`resolve_row`] matches a
/// tab title against `AgentSession::display_label`, and the title it is matching
/// was *written* from a snapshot whose names were already resolved
/// (`terminal_title::sync` runs inside `commands::emit_sessions_updated`) — so a
/// row the user renamed carries `bga-assistant` on its tab and `assistant` in the
/// raw `AppState`, and the two never meet. Caught in production the day the
/// Windows adapter first ran: a real departure logged `no_target`, which is
/// exactly what a sensor looks like when it is quietly broken.
///
/// It goes through `CustomNamesStore::apply` — the same single resolution point
/// the emit path and the notification path use — rather than
/// `commands::resolved_snapshot`, which would also merge in the remote rows
/// [`resolve_row`] then has to filter back out.
fn local_rows(app: &AppHandle) -> Vec<AgentSession> {
    let Some(state) = app.try_state::<AppState>() else { return Vec::new() };
    let mut sessions = state.snapshot();
    if let Some(names) = app.try_state::<crate::custom_names::CustomNamesStore>() {
        names.apply(&mut sessions);
    }
    sessions
}

/// Which local row a terminal observation names, or why none.
///
/// Three answers rather than an `Option`, because a caller explaining itself in
/// `widget.jsonl` needs to tell "that tab belongs to nobody here" apart from "two
/// rows answer to that name and I will not guess" — the second is a sensor that
/// is structurally dead for those rows, and logging it as the first would hide
/// that behind the ordinary case.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    Row(String),
    /// This many local rows carry the *same* label, so the title names all of
    /// them equally. Only ever reached by rows labelled identically — see
    /// [`resolve_row`].
    Ambiguous(usize),
    /// Nothing here names a row.
    Unknown,
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
/// answers [`Resolved::Unknown`] rather than guessing when neither resolves.
///
/// **The longest matching label wins, and only an exact draw is refused.**
/// `TitleReading::names` is a token-boundary prefix match, so with a
/// `projects_root` set — which turns `bga/assistant` into the label
/// `bga assistant` — the title `🟢 bga assistant` is named by *both* a row
/// labelled `bga` and its subproject row. Taking the first match, as this did,
/// marks whichever row `AppState` happens to hold first as read: the wrong row,
/// hiding unread work, and dependent on insertion order so it can flip between
/// emits. Refusing every such collision would be no better — it would kill the
/// subproject row's sensor permanently, since its label never stops being
/// prefixed. Every match here is a prefix of the *same* string, and prefixes are
/// totally ordered by length, so the longest is unique unless two rows carry a
/// literally identical label — which is the only genuinely unanswerable case and
/// the only one refused.
///
/// A refusal is a hard stop rather than a fall-through to the cwd, because
/// dropping to the working directory is exactly the hazard the title-first
/// ordering above exists to close.
pub fn resolve_row(session: &TerminalSession, sessions: &[AgentSession], projects_root: Option<&str>) -> Resolved {
    if let Some(reading) = session.title.as_deref().and_then(crate::terminal_title::parse_title) {
        let mut best: Option<&AgentSession> = None;
        let mut drawn = 1;
        for s in sessions.iter().filter(|s| s.origin.is_none()) {
            let label = s.display_label();
            if !reading.names(label) {
                continue;
            }
            match best {
                Some(b) if b.display_label().len() > label.len() => {}
                Some(b) if b.display_label().len() == label.len() => drawn += 1,
                _ => (best, drawn) = (Some(s), 1),
            }
        }
        if let Some(s) = best {
            return if drawn > 1 { Resolved::Ambiguous(drawn) } else { Resolved::Row(s.id.clone()) };
        }
    }
    let Some(cwd) = session.cwd.as_deref() else { return Resolved::Unknown };
    let derived = crate::adapters::claude::derive_chat_id(Some(cwd), projects_root);
    // A row id is unique by construction, so this join matches at most one row
    // and needs no tie-break of its own.
    match sessions.iter().find(|s| s.origin.is_none() && s.id == derived) {
        Some(s) => Resolved::Row(s.id.clone()),
        None => Resolved::Unknown,
    }
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
    let Some(mut adapter) = crate::terminals::for_platform(&app) else { return };
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
            let projects_root = watched.try_state::<crate::config::ConfigState>().and_then(|c| c.config.lock().unwrap().projects_root.clone());
            apply(&watched, &local_rows(&watched), terminal, &observation, projects_root.as_deref());
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
    let sessions = local_rows(app);
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
    let id = match resolve_row(&observation.session, sessions, projects_root) {
        Resolved::Row(id) => id,
        Resolved::Ambiguous(rows) => {
            tracing::debug!(
                decision = "attention_poll",
                terminal,
                outcome = "ambiguous_title",
                rows,
                kind = ?observation.kind,
                title = ?observation.session.title,
                "several rows carry this exact name, so the tab names all of them equally"
            );
            return;
        }
        Resolved::Unknown => {
            tracing::debug!(
                decision = "attention_poll",
                terminal,
                outcome = "no_target",
                kind = ?observation.kind,
                title = ?observation.session.title,
                "the terminal named a session matching no row"
            );
            return;
        }
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

    /// The id [`resolve_row`] settled on, for the cases that only care that one
    /// row was named. The tests that care *why* nothing was named assert on
    /// [`Resolved`] directly.
    fn row_of(session: &TerminalSession, sessions: &[AgentSession], projects_root: Option<&str>) -> Option<String> {
        match resolve_row(session, sessions, projects_root) {
            Resolved::Row(id) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn the_title_names_the_row_and_outranks_the_working_directory() {
        // The hazard this ordering exists for: a session that `cd`-ed into a
        // subdirectory reports a cwd deriving *another* row's id, and stamping
        // that row would mark unread work as read.
        let sessions = vec![row("dash", Status::Done, 0, None), row("sub", Status::Done, 0, None)];
        let s = named(Some("🟢 dash"), Some("/p/dash/sub"));
        assert_eq!(row_of(&s, &sessions, None).as_deref(), Some("dash"), "the title wins over the misleading cwd");
    }

    #[test]
    fn a_title_with_suffixes_still_names_its_row() {
        // `build_title` appends " [N%]" and " ⚠"; matching the whole string would
        // have to learn every suffix it grows later.
        let sessions = vec![row("dash", Status::Done, 0, None)];
        for title in ["🟢 dash", "🟢 dash [62%]", "🟢 dash ⚠", "🟢 dash [62%] ⚠"] {
            assert_eq!(row_of(&named(Some(title), None), &sessions, None).as_deref(), Some("dash"), "{title}");
        }
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_not_confused_for_it() {
        let sessions = vec![row("dash", Status::Done, 0, None), row("dashboard", Status::Done, 0, None)];
        assert_eq!(row_of(&named(Some("🟢 dashboard"), None), &sessions, None).as_deref(), Some("dashboard"));
        assert_eq!(row_of(&named(Some("🟢 dash"), None), &sessions, None).as_deref(), Some("dash"));
    }

    #[test]
    fn a_name_a_whole_word_longer_goes_to_the_longer_row() {
        // The collision `names`' token-boundary rule really does admit, and the
        // one a `projects_root` produces: `bga/assistant` becomes the label
        // `bga assistant`, which the sibling row `bga` also names. Taking the
        // first match marked whichever row `AppState` held first — the wrong row,
        // hiding unread work, and dependent on insertion order.
        let deep = || row("bga assistant", Status::Done, 0, None);
        let shallow = || row("bga", Status::Done, 0, None);
        for sessions in [vec![shallow(), deep()], vec![deep(), shallow()]] {
            assert_eq!(row_of(&named(Some("🟢 bga assistant"), None), &sessions, None).as_deref(), Some("bga assistant"));
            assert_eq!(row_of(&named(Some("🟢 bga assistant [62%]"), None), &sessions, None).as_deref(), Some("bga assistant"), "and through a suffix");
            // The shallow row is still perfectly resolvable from its own tab.
            assert_eq!(row_of(&named(Some("🟢 bga"), None), &sessions, None).as_deref(), Some("bga"));
        }
    }

    #[test]
    fn two_rows_named_exactly_alike_are_refused_rather_than_guessed_between() {
        // The residue longest-match cannot settle, because the labels are the
        // same string: `custom_names::set` enforces no uniqueness. Refusing marks
        // neither, which leaves both rows *showing* — the recoverable direction.
        let mut a = row("one", Status::Done, 0, None);
        let mut b = row("two", Status::Done, 0, None);
        a.display_name = Some("web".into());
        b.display_name = Some("web".into());
        assert_eq!(resolve_row(&named(Some("🟢 web"), None), &[a, b], None), Resolved::Ambiguous(2));
    }

    #[test]
    fn a_refused_title_does_not_fall_through_to_the_working_directory() {
        // Dropping to the cwd here would re-open the exact hazard the
        // title-first ordering exists to close, and would resolve the tie by
        // picking the row the ambiguous title was never able to name.
        let mut a = row("one", Status::Done, 0, None);
        let mut b = row("two", Status::Done, 0, None);
        a.display_name = Some("web".into());
        b.display_name = Some("web".into());
        assert_eq!(resolve_row(&named(Some("🟢 web"), Some("/p/one")), &[a, b], None), Resolved::Ambiguous(2));
    }

    #[test]
    fn without_a_usable_title_it_falls_back_to_the_cwd_join() {
        // `terminal_titles` can be off, and a session can predate the dashboard
        // ever writing to that tab.
        let sessions = vec![row("dash", Status::Done, 0, None)];
        assert_eq!(row_of(&named(None, Some("/p/dash")), &sessions, None).as_deref(), Some("dash"));
    }

    #[test]
    fn a_renamed_row_is_named_by_the_name_on_its_tab() {
        // A tab carries the *display* name, because that is what `build_title`
        // wrote there; the raw `AppState` row carries only its chat_id. This is
        // the invariant `local_rows` exists to hold up — the second half is the
        // production failure it was written for, where a real departure from
        // `🟢 bga-assistant` logged `no_target` against a row called `assistant`.
        let mut renamed = row("assistant", Status::Done, 0, None);
        renamed.display_name = Some("bga-assistant".into());
        assert_eq!(row_of(&named(Some("🟢 bga-assistant"), None), &[renamed], None).as_deref(), Some("assistant"));
        let unresolved = row("assistant", Status::Done, 0, None);
        assert_eq!(row_of(&named(Some("🟢 bga-assistant"), None), &[unresolved], None), None, "an unresolved snapshot cannot recognize its own tab");
    }

    #[test]
    fn an_unmatchable_session_resolves_to_nothing() {
        let sessions = vec![row("dash", Status::Done, 0, None)];
        assert_eq!(row_of(&named(Some("🟢 stranger"), Some("/p/elsewhere")), &sessions, None), None, "no row is better than the wrong row");
    }

    #[test]
    fn a_remote_row_is_never_resolved() {
        // Attention is about the human at this keyboard; another machine's rows
        // are not ours to mark read.
        let mut remote = row("dash", Status::Done, 0, None);
        remote.origin = Some("chrome".into());
        assert_eq!(row_of(&named(Some("🟢 dash"), Some("/p/dash")), &[remote], None), None);
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
