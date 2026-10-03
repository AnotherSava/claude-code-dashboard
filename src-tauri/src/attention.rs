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
//! onto the row as `read` by `commands::display_snapshot`. This module
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
//!   agterm on macOS, Windows Terminal and agwinterm on Windows — and this module
//!   never names one.
//!   What arrives here is a [`crate::terminals::Observation`]: a session named by
//!   `cwd` and `title`, an absolute instant, whether the user *left* it or typed
//!   in it, and the facts the terminal could gather about who did. Whether those
//!   facts make it a person reading the row is decided here, for every terminal
//!   alike, by [`crate::terminals::person_verdict`].
//!
//! **A tab here can be showing another machine's agent**, and then the row it
//! names is a synced one. An SSH or tmux attach renders that session in a tab on
//! this desk, so leaving it is this machine's observation to make — the far
//! machine cannot see this keyboard any more than this one can see its. Such a
//! tab is told apart by `terminal_title::REMOTE_BADGE` in front of the title,
//! which the attach puts there and this dashboard only ever reads. The badge is
//! what makes the join safe rather than merely possible: both machines routinely
//! hold a row of the same name, so it says *which* of them the tab belongs to,
//! and without it the title would name the near row and mark unread work read.
//! [`resolve_row`] therefore answers the badged case first, against synced rows
//! only, and what it finds is reported back to the origin over
//! `POST /api/sync/attention` so one row reads the same on both screens. A
//! badged title that parses as a status takes no working-directory fallback;
//! one that does not parse — the far machine has written no status yet — still
//! reaches it, as every badged title did before, and `person_verdict` refuses
//! it there because the occupant is the transport.
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
use crate::terminal_title::Named;
use crate::terminals::{NamedBy, Observation, ObservationKind, TerminalSession};
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
    /// Another machine reported that its user read one of our sessions — it had
    /// a tab rendering this agent, over SSH or tmux, and saw them leave it.
    /// Arrives over `POST /api/sync/attention`.
    PeerRead,
}

impl AttentionSource {
    fn key(self) -> &'static str {
        match self {
            Self::HistoryOpened => "history_opened",
            Self::HistoryClosed => "history_closed",
            Self::TerminalDeparted => "terminal_departed",
            Self::TerminalInput => "terminal_input",
            Self::PeerRead => "peer_read",
        }
    }
}

impl From<ObservationKind> for AttentionSource {
    fn from(kind: ObservationKind) -> Self {
        match kind {
            ObservationKind::Departed(_) => Self::TerminalDeparted,
            ObservationKind::Input(_) => Self::TerminalInput,
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
///
/// A synced row counts, because a terminal here can name one: an SSH or tmux tab
/// attached to the other machine's agent renders that session, and leaving it is
/// as real an observation as leaving a local tab. The row is unread here when
/// neither machine has seen it — its origin's verdict (`read`) and this
/// machine's own observation are both `false`.
pub fn should_poll(sessions: &[AgentSession]) -> bool {
    sessions.iter().any(|s| !s.read && s.attention() == Attention::Pending)
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
/// `commands::resolved_snapshot`, which additionally stamps facts no resolution
/// reads.
///
/// **Synced rows are in here too**, because a terminal on this machine can name
/// one: a tab that is an SSH or tmux attach onto an agent on the other machine
/// renders that agent's session, and the title it carries is the one that
/// machine's dashboard wrote, behind `terminal_title::REMOTE_BADGE`. Leaving
/// them out is what used to make such a tab unnameable — and worse, what made
/// its title fall through to the local row of the same name.
fn all_rows(app: &AppHandle) -> Vec<AgentSession> {
    let Some(state) = app.try_state::<AppState>() else { return Vec::new() };
    let mut sessions = state.snapshot();
    sessions.extend(state.remote_snapshot());
    if let Some(names) = app.try_state::<crate::custom_names::CustomNamesStore>() {
        names.apply(&mut sessions);
    }
    sessions
}

/// Which row a terminal observation names, or why none.
///
/// Three answers rather than an `Option`, because a caller explaining itself in
/// `widget.jsonl` needs to tell "that tab belongs to nobody here" apart from "two
/// rows answer to that name and I will not guess" — the second is a sensor that
/// is structurally dead for those rows, and logging it as the first would hide
/// that behind the ordinary case.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The row's id, and whether the title or the working directory named it,
    /// which the person verdict's occupant rule needs.
    Row(String, NamedBy),
    /// This many rows carry the *same* label, so the title names all of them
    /// equally. Only ever reached by rows labelled identically — see
    /// [`resolve_row`].
    Ambiguous(usize),
    /// Nothing here names a row.
    Unknown,
}

/// Which row a terminal session is, given how its terminal names it.
///
/// A badged title is answered first and against synced rows only; everything
/// below is the unbadged pass, over this machine's own rows.
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
/// **The longest matching label wins, and only an exact draw is refused**
/// ([`crate::terminal_title::title_names`], the rule `terminals::labels` joins
/// by too). Taking the first match would mark whichever row `AppState` holds
/// first as read when a title names both a row and its subproject row: the wrong
/// row, hiding unread work, and flipping with insertion order. Refusing every such
/// collision would kill the subproject row's sensor for good, since its label
/// never stops being prefixed.
///
/// A refusal is a hard stop rather than a fall-through to the cwd, because
/// dropping to the working directory is exactly the hazard the title-first
/// ordering above exists to close.
pub fn resolve_row(session: &TerminalSession, sessions: &[AgentSession], projects_root: Option<&str>) -> Resolved {
    // A tab wearing the remote badge renders a session on another machine, so
    // the only rows it can name are the synced ones — and it is answered here,
    // before the local pass and with no fall-through, because the hazard it
    // closes is precisely that the two halves share names. Both machines hold a
    // `claude` row; the badge is what says which of them the user was looking
    // at, and spending the observation on the local one would mark unread work
    // read. For the same reason there is no working-directory fallback on this
    // path: a transport opened by the attach picker reports `/`, and one started
    // by hand inside a project reports *this* machine's copy of that project —
    // the wrong row, confidently derived.
    if let Some(title) = session.title.as_deref() {
        match crate::terminal_title::title_names(title, true, remote_labels(sessions)) {
            Some(Named::One(id)) => return Resolved::Row(id.to_string(), NamedBy::RemoteTitle),
            Some(Named::Drawn(drawn)) => return Resolved::Ambiguous(drawn.len()),
            // A badged title naming no synced row: the session it belongs to has
            // ended over there, or that device has gone quiet and been reaped.
            // Nothing to credit, and the local pass must not see it.
            Some(Named::Nothing) => return Resolved::Unknown,
            None => {}
        }
    }
    let local = sessions.iter().filter(|s| s.origin.is_none()).map(|s| (s, s.display_label()));
    match session.title.as_deref().and_then(|t| crate::terminal_title::title_names(t, false, local)) {
        Some(Named::One(s)) => return Resolved::Row(s.id.clone(), NamedBy::Title),
        Some(Named::Drawn(drawn)) => return Resolved::Ambiguous(drawn.len()),
        Some(Named::Nothing) | None => {}
    }
    let Some(cwd) = session.cwd.as_deref() else { return Resolved::Unknown };
    let derived = crate::adapters::claude::derive_chat_id(Some(cwd), projects_root);
    // A row id is unique by construction, so this join matches at most one row
    // and needs no tie-break of its own.
    match sessions.iter().find(|s| s.origin.is_none() && s.id == derived) {
        Some(s) => Resolved::Row(s.id.clone(), NamedBy::Directory),
        None => Resolved::Unknown,
    }
}

/// Every name a synced row answers to, paired with its id.
///
/// **Up to three per row, because the title and the row are named by different
/// machines.** The badge in front of a tab's title is all that is added locally;
/// what follows it is the string the *origin's* `build_title` wrote, which is
/// that row's display label over there. This side holds the row as
/// `{device}/{raw_id}`, under its own display name if the user renamed it here.
/// So a title is matched against all three names the row answers to: the
/// receiver's own label, the de-namespaced id, and `AgentSession::origin_label`
/// — the name the origin's tab actually shows, carried on the push precisely
/// because this side cannot derive it.
///
/// The third is what makes the join a design rather than a coincidence. Custom
/// names are per-machine on purpose, so a row renamed on one machine only would
/// otherwise have a tab naming nothing here for the rest of that row's life —
/// and a one-sided rename is the ordinary outcome of renaming, not an edge.
///
/// The prefix is stripped by `origin` rather than by splitting on the first `/`,
/// the rule `sync::resolve_fetch_target` follows and for the same reason: a
/// device name may contain one.
///
/// Offering two names per row cannot make one row draw with itself, so a draw
/// here means what it means on the local pass: two *rows* answer equally well.
/// A row's second name is pushed only where it differs from the first, and
/// `title_names` ranks by label length over a token-boundary prefix match — so
/// two labels that tie are byte-identical, which two names of one row never are.
/// Pinned by `terminal_title`'s `a_draw_can_only_be_byte_identical_labels`,
/// since both halves of that argument are its rules rather than this module's.
fn remote_labels(sessions: &[AgentSession]) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for s in sessions.iter().filter(|s| s.origin.is_some()) {
        let mut names: Vec<&str> = vec![s.display_label()];
        if let Some(raw) = s.origin.as_deref().and_then(|o| s.id.strip_prefix(o)).and_then(|r| r.strip_prefix('/')) {
            names.push(raw);
        }
        if let Some(origin) = s.origin_label.as_deref() {
            names.push(origin);
        }
        for (i, name) in names.iter().enumerate() {
            if !name.is_empty() && !names[..i].contains(name) {
                out.push((s.id.as_str(), *name));
            }
        }
    }
    out
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
            apply(&watched, &all_rows(&watched), &observation, projects_root.as_deref());
        }
    });

    // The pull half, which brings the input observations: no snapshot file or
    // caption event carries an input clock. It does not stand in for the watch.
    // A switch found by sampling happened at an instant nobody can name, so the
    // verdict refuses every departure the poll reports, and while a watch cannot
    // run — a schema change, a missing directory, a lost hook — no departure is
    // credited at all; each adapter says so at warn when that happens.
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
    let sessions = all_rows(app);
    if !should_poll(&sessions) {
        return;
    }
    let now = crate::commands::now_ms();
    let projects_root = app.try_state::<crate::config::ConfigState>().and_then(|c| c.config.lock().unwrap().projects_root.clone());
    for observation in adapter.poll(now) {
        apply(app, &sessions, &observation, projects_root.as_deref());
    }
}

/// Resolve one observation, judge whether a person reading that row made it, and
/// stamp the row when one did.
///
/// The judgment is `terminals::person_verdict`, the same rule for every terminal,
/// applied here rather than in each adapter so a refusal reads the same in the
/// log whichever terminal saw it. It runs after the resolution because one of
/// its rules depends on how the row was named. Every observation that names a row
/// leaves exactly one line here, credited or refused; those that name none leave
/// the two below. Each names the terminal that made the observation, which on a
/// platform where several answer as one is not the adapter `spawn` was handed.
fn apply(app: &AppHandle, sessions: &[AgentSession], observation: &Observation, projects_root: Option<&str>) {
    let terminal = observation.terminal;
    let (id, named_by) = match resolve_row(&observation.session, sessions, projects_root) {
        Resolved::Row(id, named_by) => (id, named_by),
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
    let verdict = crate::terminals::person_verdict(observation, named_by);
    tracing::debug!(
        decision = "attention_poll",
        terminal,
        outcome = verdict.err().map_or("credited", |r| r.slug()),
        detail = ?verdict.err().and_then(|r| r.detail()),
        id = %id,
        named_by = ?named_by,
        at_ms = observation.at_ms,
        kind = ?observation.kind,
        occupant = ?observation.occupant,
        title = ?observation.session.title,
        "person verdict"
    );
    if verdict.is_ok() {
        observe(app, &id, observation.at_ms, observation.kind.into());
    }
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
    // A local row first, then the synced ones — two stores, so two lookups.
    if state.mark_attended(id, at_ms) {
        tracing::debug!(id = %id, decision = "attention_seen", source = source.key(), attended_at = at_ms, "session marked as read");
        crate::commands::emit_sessions_updated(app);
        return true;
    }
    // The instant goes in but is not what gets stamped: a synced row's
    // `content_at` is the origin's clock, so `at_ms` is weighed against when
    // this device saw that content arrive and the row is stamped with the
    // origin's own watermark. See `AppState::mark_remote_attended`.
    let Some(stamp) = state.mark_remote_attended(id, at_ms) else { return false };
    tracing::debug!(id = %id, decision = "attention_seen", source = source.key(), attended_at = stamp, remote = true, "synced session marked as read on this machine");
    // Tell the machine that owns it, so its own widget, its tab and its Telegram
    // alerts agree rather than still asking about something read here. Sent once
    // and not retried: a lost report leaves that row showing over there, which is
    // the recoverable direction, and a retry would need a record of what the
    // origin had acknowledged — a guess about another device's contents, which
    // this module's own history says goes stale silently.
    crate::sync::report_attention(app, id, stamp);
    // The remote variant: this touched only `AppState::remote`, which the pusher
    // never ships, so the local emitter's `SyncDirty` poke would schedule a push
    // carrying nothing new — and its terminal-title pass has no local row to
    // reconcile either. Telling the origin is `report_attention`'s job, above.
    crate::commands::emit_sessions_updated_remote(app);
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
                turn_from_relay: None,
                delegated_task: None,
                message_line: None,
                message_is_reply: None,
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
            Resolved::Row(id, _) => Some(id),
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
        // the invariant `all_rows` exists to hold up — the second half is the
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

    /// A synced row as this machine holds it: namespaced, origin-stamped, and
    /// optionally renamed here.
    fn synced(device: &str, raw: &str, renamed: Option<&str>) -> AgentSession {
        let mut s = row(&format!("{device}/{raw}"), Status::Done, 0, None);
        s.origin = Some(device.into());
        s.display_name = renamed.map(str::to_string);
        s
    }

    #[test]
    fn an_unbadged_title_never_names_a_synced_row() {
        // The tab is a local one. Its title is the string *this* dashboard wrote,
        // and only a local row can be behind it — which is what stops a title
        // from naming the other machine's row of the same name.
        let remote = synced("chrome", "dash", None);
        assert_eq!(row_of(&named(Some("🟢 dash"), Some("/p/dash")), &[remote], None), None);
    }

    #[test]
    fn a_badged_title_names_the_synced_row_by_its_de_namespaced_id() {
        // What the other machine's dashboard wrote is `🟢 claude`; the attach
        // puts the badge in front. This side files the row as `chrome/claude`,
        // so the id is what the two agree on.
        let rows = vec![synced("chrome", "claude", None)];
        let s = named(Some("⇄ 🟢 claude"), Some("/"));
        assert_eq!(resolve_row(&s, &rows, None), Resolved::Row("chrome/claude".into(), NamedBy::RemoteTitle));
    }

    #[test]
    fn a_badged_title_names_the_synced_row_by_a_name_renamed_on_both_machines() {
        // The other machine titles the tab from *its* display name, so a row
        // renamed there carries a name this side only recognizes because it was
        // renamed here too. Both names are offered; whichever agrees wins.
        let rows = vec![synced("chrome", "tauri-dashboard", Some("ai-dashboard"))];
        let s = named(Some("⇄ ⚫ ai-dashboard"), Some("/"));
        assert_eq!(resolve_row(&s, &rows, None), Resolved::Row("chrome/tauri-dashboard".into(), NamedBy::RemoteTitle));
    }

    #[test]
    fn a_row_renamed_to_its_own_raw_id_is_offered_one_name_not_two() {
        // `remote_labels` pushes the de-namespaced id only where it differs from
        // the display label, which is what keeps one row from ever competing with
        // itself — the reason a draw always means two rows (pinned at its source
        // by `terminal_title`'s `a_draw_can_only_be_byte_identical_labels`).
        let rows = vec![synced("chrome", "claude", Some("claude"))];
        assert_eq!(remote_labels(&rows).len(), 1);
        assert_eq!(resolve_row(&named(Some("⇄ 🟢 claude"), None), &rows, None), Resolved::Row("chrome/claude".into(), NamedBy::RemoteTitle));
    }

    #[test]
    fn a_badged_title_two_devices_answer_to_is_refused() {
        let rows = vec![synced("chrome", "web", None), synced("desk", "web", None)];
        assert_eq!(resolve_row(&named(Some("⇄ 🟢 web"), None), &rows, None), Resolved::Ambiguous(2));
    }

    #[test]
    fn a_badged_title_never_falls_through_to_a_local_row_of_the_same_name() {
        // The hazard the badge exists for, and the reason this path has no
        // working-directory fallback either. Both machines have `claude`; the
        // tab is the far one. Crediting the near row would mark unread work
        // read, the one direction this feature refuses — and the attach picker
        // opens such a tab in `/`, while one started by hand inside the project
        // reports *this* machine's copy of it.
        let rows = vec![row("claude", Status::Done, 0, None), synced("chrome", "claude", None)];
        assert_eq!(resolve_row(&named(Some("⇄ 🟢 claude"), Some("/")), &rows, None), Resolved::Row("chrome/claude".into(), NamedBy::RemoteTitle));
        assert_eq!(resolve_row(&named(Some("⇄ 🟢 claude"), Some("/p/claude")), &rows, None), Resolved::Row("chrome/claude".into(), NamedBy::RemoteTitle));
        // And once that session has ended over there, the tab names nothing —
        // never the local row that is still going.
        let local_only = vec![row("claude", Status::Done, 0, None)];
        assert_eq!(resolve_row(&named(Some("⇄ 🟢 claude"), Some("/p/claude")), &local_only, None), Resolved::Unknown);
    }

    #[test]
    fn a_row_renamed_on_the_origin_alone_is_still_named_by_its_tab() {
        // Custom names are per-machine, so this is the ordinary outcome of
        // renaming rather than an edge. The tab carries the *origin's* name; this
        // side holds neither it nor a matching local rename, so without
        // `origin_label` on the wire the title would name nothing here for the
        // rest of the row's life.
        let mut row = synced("chrome", "assistant", None);
        row.origin_label = Some("bga-assistant".into());
        let rows = vec![row];
        assert_eq!(resolve_row(&named(Some("⇄ ⚫ bga-assistant"), None), &rows, None), Resolved::Row("chrome/assistant".into(), NamedBy::RemoteTitle));
        // Its other two names keep working, so a rename undone on the far side
        // does not strand the tab either.
        assert_eq!(resolve_row(&named(Some("⇄ ⚫ assistant"), None), &rows, None), Resolved::Row("chrome/assistant".into(), NamedBy::RemoteTitle));
        assert_eq!(remote_labels(&rows).len(), 3, "all three names, each once");
    }

    #[test]
    fn a_name_offered_twice_is_listed_once() {
        // The origin and this machine renaming a row alike, which is the live
        // configuration: three sources, two distinct strings. A duplicate would
        // be one row competing with itself for the title.
        let mut row = synced("chrome", "tauri-dashboard", Some("ai-dashboard"));
        row.origin_label = Some("ai-dashboard".into());
        assert_eq!(remote_labels(std::slice::from_ref(&row)), vec![("chrome/tauri-dashboard", "ai-dashboard"), ("chrome/tauri-dashboard", "tauri-dashboard")]);
    }

    #[test]
    fn the_unbadged_pass_never_reaches_a_synced_row() {
        // The filter that keeps the two passes apart, and it carries weight it
        // did not before: `all_rows` now hands this function synced rows too, so
        // without it an ordinary local tab's title could name another machine's
        // row — and the badged pass having already declined is exactly when that
        // would happen.
        let rows = vec![synced("chrome", "web", None)];
        assert_eq!(resolve_row(&named(Some("🟢 chrome/web"), None), &rows, None), Resolved::Unknown);
        assert_eq!(resolve_row(&named(Some("🟢 web"), Some("/p/web")), &rows, None), Resolved::Unknown, "nor through the directory");
    }

    #[test]
    fn an_unbadged_tab_still_names_its_local_row_with_synced_rows_present() {
        // The other direction of the same split, and the one a badge-only test
        // would not catch: the near machine's own tab must keep working while a
        // row of that name exists on both.
        let rows = vec![row("claude", Status::Done, 0, None), synced("chrome", "claude", None)];
        assert_eq!(resolve_row(&named(Some("🟢 claude"), Some("/p/claude")), &rows, None), Resolved::Row("claude".into(), NamedBy::Title));
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
    fn an_unread_synced_row_makes_this_machine_ask_too() {
        // A terminal here can name one: an SSH or tmux tab rendering the other
        // machine's agent is a tab on this desk, and leaving it is read.
        let mut remote = row("a", Status::Done, 1_000_000, None);
        remote.origin = Some("chrome".into());
        assert!(should_poll(std::slice::from_ref(&remote)));

        // Already read on the machine that runs it, so there is nothing here to
        // find out — the verdict arrived with the row.
        remote.read = true;
        assert!(!should_poll(&[remote]));
    }

    #[test]
    fn every_observation_kind_maps_to_a_distinct_logged_source() {
        assert_eq!(AttentionSource::from(crate::terminals::verdict_tests::departure().kind).key(), "terminal_departed");
        assert_eq!(AttentionSource::from(crate::terminals::verdict_tests::input().kind).key(), "terminal_input");
    }

    #[test]
    fn the_resolution_says_whether_the_title_or_the_directory_named_the_row() {
        // The person verdict's occupant rule turns on this: a row named only
        // through a working directory may have been named by a plain shell.
        let sessions = vec![row("dash", Status::Done, 0, None)];
        assert_eq!(resolve_row(&named(Some("🟢 dash"), Some("/p/dash")), &sessions, None), Resolved::Row("dash".into(), NamedBy::Title));
        assert_eq!(resolve_row(&named(Some("zsh"), Some("/p/dash")), &sessions, None), Resolved::Row("dash".into(), NamedBy::Directory));
    }
}
