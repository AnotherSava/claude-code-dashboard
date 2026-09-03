//! Terminal adapters: what the dashboard asks a terminal, and how each one
//! answers.
//!
//! One adapter per terminal, because the answers are terminal-specific and always
//! will be — agterm on macOS, Windows Terminal plus the Windows console on
//! Windows. An adapter's whole job is to turn whatever its terminal exposes into
//! this module's vocabulary; everything downstream is generic and names no
//! terminal at all. The two answer the same two questions from entirely different
//! places: agterm from a control socket and a state file, Windows from a window
//! title and a console object.
//!
//! Two questions, two callers:
//!
//! - **Did the user look at this session?** [`TerminalAdapter::poll`] and
//!   [`TerminalAdapter::watch`] answer it as a stream of [`Observation`]s.
//!   `crate::attention` consumes them — resolving each to a row, deciding the
//!   [`crate::state::Attention`] verdict, logging and emitting.
//! - **What are you showing right now?** [`TerminalAdapter::sessions`] answers it
//!   as a plain list. `crate::session_restore` consumes it to give a live session
//!   its row back after a restart, reading the status out of the tab title this
//!   dashboard last wrote there.
//!
//! They are different axes — one is about a human, the other about a screen — and
//! they share the seam because they share all of its vocabulary.
//!
//! Two things make that seam hold rather than leak:
//!
//! - **A session is named by `cwd` and `title`,** not by the terminal's own id.
//!   Every terminal has both, and both mean the same thing everywhere; a window
//!   id or a pane handle would not survive the next adapter.
//! - **An observation carries an absolute instant,** never "just now" or a
//!   duration. Terminals report freshness in different shapes (agterm gives an
//!   idle clock, another might give a timestamp or an event), so converting to an
//!   instant is the adapter's job — and doing it there is what lets a late or
//!   coalesced reading stay correct rather than merely recent.
//!
//! Not part of the seam, on purpose: *when* to ask. The tick and its gate live in
//! `crate::attention`, so a new adapter inherits the "spend nothing while every
//! finished row is already read" policy instead of re-deciding it.

#[cfg(target_os = "macos")]
pub mod agterm;
#[cfg(target_os = "windows")]
pub mod windows;

/// The instant to credit a departure to, or `None` when nobody left.
///
/// Shared by every adapter because both of its rules are, and a second copy of
/// either would be a second thing to get wrong.
///
/// A departure is a *change* of selection: a different session is on screen now,
/// so the user left the previous one somewhere in between. That is what marks a
/// row read — leaving is the moment you are done with what was on screen, and
/// reading itself produces no keystroke to observe.
///
/// The stamp is the **previous** reading rather than now, because the change is
/// known only to within an interval and crediting it to `now` would mark a row
/// that finished *during* that interval as read by a departure that came before
/// it. A first observation is deliberately not a departure — at startup every
/// window's selection is new to us, and there is no earlier session to have left.
///
/// The selection key is the adapter's own and the adapters do not agree on what
/// it is, which is why `same_selection` is a parameter rather than `==`. agterm
/// keys on agterm's session id, a stable handle that says nothing about titles;
/// Windows Terminal publishes no handle at all, so its adapter keys on the tab
/// title and has to ask `terminal_title::same_row` whether two of them mean one
/// row. Both keys are strings and neither is a dashboard row id.
pub fn departure_stamp(previous: Option<&str>, current: &str, same_selection: impl Fn(&str, &str) -> bool, last_reading_at: Option<i64>, now_ms: i64) -> Option<i64> {
    let switched = previous.is_some_and(|p| !same_selection(p, current));
    switched.then(|| last_reading_at.unwrap_or(now_ms))
}

/// A terminal session as its *terminal* names it — the two handles every terminal
/// has, and the only ones `attention::resolve_row` needs to find a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSession {
    /// The session's working directory.
    pub cwd: Option<String>,
    /// The session's raw terminal title. On this machine that is the string
    /// `terminal_title::build_title` wrote, so it names the row directly — which
    /// is why `resolve_row` prefers it over `cwd`.
    pub title: Option<String>,
}

/// What the user was observed doing.
///
/// `dead_code` is allowed because this is the seam's vocabulary, not one
/// terminal's: on a platform whose adapter is not written — Linux, where
/// [`for_platform`] answers `None` — nothing constructs these, and that is the
/// expected state rather than a defect. Deleting them to silence it would delete
/// the interface the next adapter implements.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationKind {
    /// The user **left** this session's tab, having been in it. The primary
    /// signal, because leaving is the moment you are done with what was on
    /// screen — whereas arriving proves only that you got there, and reading
    /// itself produces nothing at all to observe.
    Departed,
    /// The user produced input while this session was the one on screen. Weaker
    /// and secondary: it cannot see a silent read, and it is here for the case
    /// the user reads a finished answer and then types the next prompt without
    /// ever switching away.
    Input,
}

/// One thing a terminal observed, at a known instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub session: TerminalSession,
    /// When it happened, absolute.
    ///
    /// **Err early.** A stamp later than the truth marks content that arrived
    /// after the user had gone, which hides it; a stamp earlier than the truth
    /// only leaves a row still showing, which the next observation corrects. An
    /// adapter that knows an event only to within a window must report the start
    /// of that window, not its end.
    pub at_ms: i64,
    pub kind: ObservationKind,
}

/// A terminal this dashboard can ask about.
///
/// Two questions, both answered in the vocabulary above. *What did the user do*
/// ([`poll`](TerminalAdapter::poll) / [`watch`](TerminalAdapter::watch)) and
/// *what are you showing* ([`sessions`](TerminalAdapter::sessions)). They are
/// different axes — one is about a human, the other about the screen — but they
/// share the seam because they share its whole vocabulary: a session named by
/// `cwd` and `title`, which is all either caller needs and all any terminal can
/// be relied on to have.
pub trait TerminalAdapter: Send {
    /// Stable slug for the decision log, so `widget.jsonl` says which terminal
    /// answered.
    fn name(&self) -> &'static str;

    /// Every session this terminal is showing right now, or `None` when it could
    /// not be asked.
    ///
    /// The `Option` is the point of the return type, and it draws the same line
    /// `session_registry::live_sessions` draws: `Some(vec![])` means "I have no
    /// sessions", `None` means "I could not look" — the terminal is not running,
    /// the control channel refused, the answer did not parse. Flattening them
    /// would turn a terminal that has not started yet into a machine with no
    /// tabs, and a caller reading that as fact would conclude there is nothing
    /// to restore. A check that never ran must not read as one that passed.
    ///
    /// **Listing, not observing.** This reports the terminal's own current
    /// contents; it makes no claim about a human and produces no
    /// [`Observation`]. In particular a `title` here is whatever the tab holds
    /// *now*, which — where this dashboard writes titles — is the last status it
    /// published for that session before it was last restarted.
    fn sessions(&self) -> Option<Vec<TerminalSession>>;

    /// Everything observed since the previous call. An empty vec is the normal,
    /// common answer and is not an error.
    ///
    /// `now_ms` is passed in rather than read here so the caller's clock is the
    /// only one in play, and so a test can drive the adapter without one.
    fn poll(&mut self, now_ms: i64) -> Vec<Observation>;

    /// Start pushing observations the moment they happen, if this terminal can be
    /// watched rather than asked. Called once at startup; the adapter owns
    /// whatever thread it needs and keeps it alive for the process.
    ///
    /// This exists because [`poll`](Self::poll) can only ever find out *late*.
    /// Sampling "which tab is selected" discovers a departure at the next tick, so
    /// the interval is pure discovery lag — and a visit shorter than it is missed
    /// entirely. A terminal that writes its selection somewhere observable can
    /// report the edge instead, which is both immediate and complete.
    ///
    /// Default: no push. A terminal with only a pull interface is not broken, it
    /// is just late; `poll` remains the whole story there.
    fn watch(&self, _sink: std::sync::mpsc::Sender<Observation>) {}
}

/// The adapter for this platform, or `None` where no terminal is wired up.
///
/// `app` is taken because an adapter may need state this process already holds:
/// the Windows one reads `SessionRegistry`, whose whole design is that one cache
/// serves every reader, so constructing a second would double the directory reads
/// and the process-table snapshots it exists to share.
pub fn for_platform(app: &tauri::AppHandle) -> Option<Box<dyn TerminalAdapter>> {
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        Some(Box::new(agterm::AgtermAdapter::default()))
    }
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(windows::WindowsAdapter::new(app.clone())))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = app;
        None
    }
}
