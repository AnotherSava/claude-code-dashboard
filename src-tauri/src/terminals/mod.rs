//! Terminal adapters: what the dashboard asks a terminal, and how each one
//! answers.
//!
//! One adapter per terminal, because the answers are terminal-specific and always
//! will be — agterm on macOS, Windows Terminal plus the Windows console on
//! Windows. An adapter's whole job is to turn whatever its terminal exposes into
//! this module's vocabulary; everything downstream is generic and names no
//! terminal at all. The two answer from entirely different places: agterm from a
//! control socket and a state file, Windows from a window title, a console
//! object and UI Automation.
//!
//! Four questions, four callers:
//!
//! - **Did the user look at this session?** [`TerminalAdapter::poll`] and
//!   [`TerminalAdapter::watch`] answer it as a stream of [`Observation`]s.
//!   `crate::attention` consumes them — resolving each to a row, deciding the
//!   [`crate::state::Attention`] verdict, logging and emitting.
//! - **What are you showing right now?** [`TerminalAdapter::sessions`] answers it
//!   as a plain list. `crate::session_restore` consumes it to give a live session
//!   its row back after a restart, reading the status out of the tab title this
//!   dashboard last wrote there.
//! - **What are you displaying, and what is really in there?**
//!   [`TerminalAdapter::front_readings`] answers it as a [`FrontReading`] per
//!   surface. `crate::terminals::stale_check` consumes them to notice a tab that
//!   has stopped tracking its session, which a terminal can be wrong about in a
//!   way no write reports.
//! - **Where did that write land?** [`TerminalAdapter::attached_surface`] answers
//!   it as one opaque surface key. `crate::terminal_title` consumes it while a
//!   title write is still in flight, so every surface-keyed reader — the write's
//!   own record, the caption watch, the stale check — names a surface the same
//!   way.
//!
//! They are different axes — the first is about a human, the second about a
//! screen, the third about the gap between what a screen shows and what is behind
//! it, the fourth about where this dashboard's own writing goes — and they share
//! the seam because they share all of its vocabulary.
//!
//! **A fifth capability goes here too, not beside here.** Every terminal-specific
//! fact this dashboard learns belongs behind this trait, however Windows-shaped
//! the first implementation looks. The staleness check was originally written the
//! other way — a `#[cfg(windows)]` module reaching straight into the Windows
//! adapter's internals and called by name from `terminal_title::sync` — and it
//! worked, which is exactly why the mistake is worth naming: it silently made
//! "add a second terminal" mean "rewrite the caller" instead of "write an
//! adapter".
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
/// Reading Windows Terminal's tab titles through UI Automation. Split out of
/// `windows.rs` because it is the one place this project uses the `windows` crate
/// rather than hand-declared signatures, and that file is long enough. It is
/// `pub` only because a `#[cfg]`-gated sibling module must be, and belongs to the
/// Windows adapter: nothing generic may call it.
#[cfg(target_os = "windows")]
pub mod wt_tabs;
/// The consumer of [`TerminalAdapter::front_readings`]. Names no terminal, and is
/// not platform-gated: a platform with no adapter never starts it, and an adapter
/// that cannot answer stands it down.
pub mod stale_check;

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
/// Four questions, all answered in the vocabulary above. *What did the user do*
/// ([`poll`](TerminalAdapter::poll) / [`watch`](TerminalAdapter::watch)), *what
/// are you showing* ([`sessions`](TerminalAdapter::sessions)), *what are you
/// displaying versus what is really in there*
/// ([`front_readings`](TerminalAdapter::front_readings)), and *where did that
/// write land* ([`attached_surface`](TerminalAdapter::attached_surface)). They
/// are different axes — a human, a screen, the gap between a screen and what is
/// behind it, and where this dashboard's own writing goes — but they share the
/// seam because they share its whole vocabulary: a session named by `cwd` and
/// `title`, which is all any caller needs and all any terminal can be relied on
/// to have.
///
/// Only `name` is required. Every other method has a default that declines to
/// answer, so an adapter implements what its terminal can actually tell it and
/// each caller learns the difference between a `None` and a finding.
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

    /// Every surface this terminal has in front of a user, paired with what the
    /// session in it actually is. `crate::terminals::stale_check` consumes these
    /// to notice a surface that has stopped showing its session's status.
    ///
    /// The third question, and the one whose answer a terminal can *lie* about:
    /// **what are you displaying, and what is really in there?** Where a terminal
    /// lets a user rename a tab, the displayed name stops tracking the session and
    /// nothing about the write says so — Windows Terminal keeps accepting
    /// `SetConsoleTitleW` and keeps reading the value back while the tab shows
    /// something else entirely. Only the terminal itself can be asked for both
    /// sides, which is why this is on the seam rather than derived from what this
    /// dashboard remembers writing.
    ///
    /// The default is `None`, meaning this terminal cannot be asked the question
    /// at all, the same line [`sessions`](Self::sessions) draws: a
    /// failure to look, never a finding. It is what
    /// [`stale_check::spawn`](crate::terminals::stale_check::spawn) reads to
    /// stand down instead of announcing a checker that would read nothing for
    /// the life of the process. `Some(vec![])` is the other answer entirely, and
    /// an ordinary one: the terminal was asked and has no surface in front of
    /// anybody.
    fn front_readings(&self) -> Option<Vec<FrontReading>> {
        None
    }

    /// What to tell the user when one of this terminal's surfaces is found
    /// stale, in their terminal's own vocabulary.
    ///
    /// On the seam because the remedy is as terminal-specific as the fault: only
    /// this adapter knows whether the fix is a menu item, a keybinding, or
    /// nothing at all. A generic default keeps a new adapter honest rather than
    /// silent.
    ///
    /// **Three requirements, because two consumers splice this differently.**
    /// `notifications::build_stale_tab_message` puts it after a colon and appends
    /// a full stop; `stale_check` emits it as a standalone `remedy` log field.
    /// So it must start lowercase, must not end in terminal punctuation, and must
    /// read as a clause rather than a sentence. The natural thing to write —
    /// `"Right-click the tab and choose Reset tab title."` — renders with a
    /// capital mid-sentence and a doubled period, and nothing would catch it, so
    /// `every_remedy_splices_into_a_sentence` asserts all three.
    ///
    /// **Lead with the action, then the reason**
    /// (`feedback_warning_leads_with_instruction`): the reader is looking at a
    /// phone notification and needs the four words that tell them what to do, not
    /// a diagnosis to read past first.
    ///
    /// One remedy per adapter, which is a real ceiling where an adapter spans
    /// more than one terminal — `windows` covers VS Code's terminal and a bare
    /// conhost as well. It holds today only because both things that raise the
    /// flag are scoped to Windows Terminal's own windows.
    fn stale_remedy(&self) -> &'static str {
        FALLBACK_STALE_REMEDY
    }

    /// Which of this terminal's surfaces renders the console or tty a title has
    /// just been written to, or `None` when that is not a surface this terminal
    /// recognizes.
    ///
    /// The `pid` is the process whose console or tty was just written to, and it
    /// is a parameter rather than something the implementation digs up, because
    /// otherwise only a terminal whose write target is *ambient process state*
    /// could answer at all: the Windows body works off the console this process
    /// is attached to, which the caller has just attached, and nothing else has
    /// that property. A tty write's target is the pid and only the pid, and an
    /// adapter asked without one could do no better than name the frontmost
    /// window — wrong precisely when the write went to a background one.
    ///
    /// **Only meaningful from inside the write**, while whatever the write set up
    /// is still current, so asking later answers about some other session or
    /// about nothing.
    ///
    /// It exists because a write is the only moment the mapping is knowable. The
    /// caller holds a pid and a title and nothing that says which surface renders
    /// them; the terminal holds that. The key is a [`FrontReading::surface`] and
    /// is minted by the same adapter, which is the point: the write's own record,
    /// the caption watch and [`stale_check`] then all name one surface the same
    /// way, so a title written here can be compared against a caption seen there.
    ///
    /// **It must be cheap and must not block.** The caller holds a process-wide
    /// lock that serialises every console attach in this app, and it is detached
    /// from its own console while doing so, so a slow answer stalls every title
    /// write and every title read — and an implementation that reached back into
    /// `terminal_title` would deadlock on a mutex that is not reentrant. This is
    /// the opposite of [`front_readings`](Self::front_readings), which is
    /// expected to be a slow cross-process read and is called from a thread of
    /// its own for exactly that reason.
    ///
    /// The default is `None`, meaning this terminal cannot attribute a write to a
    /// surface. Every reader must treat absence as *do not know* and never as
    /// "not in this terminal": a write that never happened leaves no entry
    /// either.
    fn attached_surface(&self, pid: u32) -> Option<String> {
        let _ = pid;
        None
    }

    /// Prepare the calling thread for [`front_readings`](Self::front_readings),
    /// once, before the first call. On the seam rather than beside it because
    /// what a reader thread needs is the adapter's business: Windows joins a COM
    /// apartment here, and a terminal read over a socket or a file needs nothing.
    fn prepare_reader(&self) {}
}

/// The remedy wording for a terminal that has not written its own.
///
/// A separate const rather than an inline literal because two places name it:
/// the [`TerminalAdapter::stale_remedy`] default, which is what a real adapter
/// that has not written its own would serve, and the alert builder's fallback for
/// a platform with no adapter at all. Only the first can reach a user today —
/// nothing raises the stale flag where there is no adapter — so the second is
/// defensive, and is here because the `map_or` needs a value rather than because
/// anyone will read it.
pub const FALLBACK_STALE_REMEDY: &str = "check whether the tab was renamed, and reset its title";

/// One surface a terminal has in front of a user, and the two strings a
/// stale-surface check compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontReading {
    /// Which surface this is, opaque to every caller.
    ///
    /// The one place the seam admits a terminal's own identifier, and it is
    /// admitted only because nothing downstream interprets it: the caller may
    /// compare two of these for equality, to remember what it saw last time, and
    /// may do nothing else with it. It deliberately does **not** name a session —
    /// that stays `cwd` and `title` per this module's rule — so a terminal with
    /// panes, spaces or no windows at all can mint whatever is stable for it.
    pub surface: String,
    /// What the surface is displaying. This is the untrusted string: a user may
    /// have overwritten it, and it is only ever the *object* of the comparison,
    /// never the thing that names a row.
    pub shown: String,
    /// The real titles of the sessions in that surface, as the terminal knows
    /// them rather than as this dashboard remembers writing them.
    ///
    /// Two levels of `Option`, and both carry weight. The outer one draws the
    /// line [`TerminalAdapter::sessions`] draws: `None` means the terminal could
    /// not be asked, which is a failure to look and never a finding. The inner
    /// one marks a session whose title could not be read, which must still
    /// occupy a slot — **the length is the split answer**, so dropping an
    /// unreadable session would turn a split surface into a single-session
    /// reading and let the rule judge one it should have abstained on.
    ///
    /// An empty title is a real reading, not a missing one, and is kept: this
    /// dashboard writes empty titles itself when it blanks a departed row.
    pub sessions: Option<Vec<Option<String>>>,
}

/// What one [`FrontReading`] establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// The surface is showing its own session's title. Carries that title.
    ///
    /// It is carried even though it equals `shown`, so that a caller naming a row
    /// has a `real` to reach for on *both* arms. The rule is that a row is named
    /// from the real title and never from the displayed one; with nothing to name
    /// here, the caller had to pass `shown` and rely on the two being equal,
    /// which reads as a breach of the discipline every time anyone checks.
    Following(String),
    /// It is showing something else.
    Diverged {
        /// What the surface displays. The untrusted string, and only ever the
        /// object of the comparison.
        shown: String,
        /// What is actually in the surface, as the terminal knows it.
        real: String,
    },
    /// Nothing could be established.
    Unknown {
        /// For the decision log.
        reason: &'static str,
        /// Whether the *surface* answered. Two of these reasons are positive
        /// knowledge rather than a failure to look — it has several panes, or
        /// none — and a caller that has recorded something about a row can then
        /// conclude that row is not in front. The rest mean the read failed, and
        /// a failed read is never grounds to discard what was already observed.
        surface_answered: bool,
    },
}

/// The stale-surface rule. Pure, cross-terminal, and the whole judgment: every
/// terminal-specific fact was turned into a [`FrontReading`] by its adapter.
///
/// It compares two *positive* readings rather than arguing from absence, which is
/// what keeps it small. An earlier design asked whether the title this dashboard
/// wrote appeared anywhere among a window's tabs; absence has too many innocent
/// causes — a write in flight, a split, a truncated enumeration, an unreadable
/// terminal, a console something else retitled, a profile that suppresses
/// application titles, several sessions on one row — and review after review
/// showed each gate added to exclude one either over-blocked into never firing or
/// under-blocked into accusing a healthy surface. Here five of those seven stop
/// being expressible: whatever owns the title appears on *both* sides, so they
/// agree; a lost reading abstains; and a split is counted rather than inferred.
pub fn read_front(r: &FrontReading) -> Reading {
    // "Could not look" outranks every other abstention, so it is tested first:
    // an unreadable surface also has nothing to show, and reporting that as
    // "showed nothing" would describe the terminal instead of the failure.
    let Some(sessions) = r.sessions.as_deref() else {
        return Reading::Unknown { reason: "surface_unreadable", surface_answered: false };
    };
    if r.shown.is_empty() {
        return Reading::Unknown { reason: "nothing_shown", surface_answered: false };
    }
    match sessions {
        [] => Reading::Unknown { reason: "no_session_in_surface", surface_answered: true },
        // A surface holding one session, whose title we could read, is the only
        // shape this can judge. Everything else abstains, and each says why.
        [Some(only)] if *only == r.shown => Reading::Following(only.clone()),
        [Some(only)] => Reading::Diverged { shown: r.shown.clone(), real: only.clone() },
        [None] => Reading::Unknown { reason: "session_title_unreadable", surface_answered: false },
        _ => Reading::Unknown { reason: "split_surface", surface_answered: true },
    }
}

#[cfg(test)]
mod stale_tests {
    use super::*;

    /// Every remedy this build compiles must splice into
    /// `notifications::build_stale_tab_message`'s sentence, which puts it after
    /// a colon and appends a full stop. Asserted over the concrete strings
    /// rather than through the trait, because an adapter needs an `AppHandle` to
    /// construct; the list is also the reminder to add the next one.
    #[test]
    fn every_remedy_splices_into_a_sentence() {
        let mut remedies = vec![FALLBACK_STALE_REMEDY];
        #[cfg(target_os = "windows")]
        remedies.push(windows::STALE_REMEDY);
        for r in remedies {
            assert!(!r.is_empty(), "a remedy that says nothing is worse than the default");
            assert!(!r.chars().next().unwrap().is_uppercase(), "must read as a mid-sentence clause, got {r:?}");
            assert!(!r.ends_with(['.', '!', '?']), "the caller appends the full stop, got {r:?}");
        }
    }

    fn reading(shown: &str, sessions: Option<&[&str]>) -> FrontReading {
        FrontReading {
            surface: "s1".into(),
            shown: shown.into(),
            sessions: sessions.map(|s| s.iter().map(|x| Some(x.to_string())).collect()),
        }
    }

    #[test]
    fn a_surface_showing_its_own_sessions_title_is_following() {
        assert_eq!(read_front(&reading("⚪ transcripts", Some(&["⚪ transcripts"]))), Reading::Following("⚪ transcripts".into()));
    }

    #[test]
    fn the_measured_fault_diverges() {
        // Captured live 2026-09-03 from a Windows Terminal tab renamed by hand:
        // the tab said `ttt` while the pane behind it was a session blocked on
        // its user. This case is the whole reason the module exists.
        assert_eq!(
            read_front(&reading("ttt", Some(&["✋ what-is-next [78%]"]))),
            Reading::Diverged { shown: "ttt".into(), real: "✋ what-is-next [78%]".into() }
        );
    }

    #[test]
    fn an_unreadable_surface_is_a_failure_to_look_not_a_finding() {
        // An elevated terminal, or any transport failure. Absence of an answer is
        // not absence of agreement.
        assert_eq!(read_front(&reading("⚪ x", None)), Reading::Unknown { reason: "surface_unreadable", surface_answered: false });
        assert_eq!(read_front(&reading("", Some(&["⚪ x"]))), Reading::Unknown { reason: "nothing_shown", surface_answered: false });
    }

    #[test]
    fn a_session_whose_title_could_not_be_read_is_not_dropped() {
        // The count is the split answer, so an unreadable session has to keep its
        // slot. Dropping it would turn a split surface into a single-session
        // reading and let the rule accuse a tab it should have abstained on —
        // and the condition is stable, so the two-sample rule could not absorb it.
        let one_unreadable = FrontReading { surface: "s1".into(), shown: "⚪ x".into(), sessions: Some(vec![None]) };
        assert_eq!(read_front(&one_unreadable), Reading::Unknown { reason: "session_title_unreadable", surface_answered: false });
        let split_with_one_unreadable = FrontReading { surface: "s1".into(), shown: "⚪ x".into(), sessions: Some(vec![Some("⚪ x".into()), None]) };
        assert_eq!(read_front(&split_with_one_unreadable), Reading::Unknown { reason: "split_surface", surface_answered: true });
    }

    #[test]
    fn an_empty_title_is_a_reading_not_a_gap() {
        // This dashboard writes empty titles itself when it blanks a departed
        // row, so an empty string is a real answer and must not be mistaken for
        // a pane that could not be read.
        let r = FrontReading { surface: "s1".into(), shown: "⚪ x".into(), sessions: Some(vec![Some(String::new())]) };
        assert_eq!(read_front(&r), Reading::Diverged { shown: "⚪ x".into(), real: String::new() });
    }

    #[test]
    fn a_split_and_a_sessionless_surface_are_counted_not_guessed() {
        // Both were false-accusation routes in the design this replaced, which
        // inferred them from window-wide totals that two anomalies could cancel
        // out. Here the adapter reports what is actually in the surface.
        assert_eq!(read_front(&reading("⚪ x", Some(&["a", "b"]))), Reading::Unknown { reason: "split_surface", surface_answered: true });
        assert_eq!(read_front(&reading("Settings", Some(&[]))), Reading::Unknown { reason: "no_session_in_surface", surface_answered: true });
    }

    #[test]
    fn whatever_owns_the_title_appears_on_both_sides() {
        // The property that removed five gates. A shell's own `PS1`, Claude
        // Code's OSC titling, a profile that suppresses application titles: each
        // acts upstream of both readings, so they move together and the rule sees
        // agreement rather than a fault it would have had to be taught to excuse.
        assert_eq!(read_front(&reading("MINGW64:/d/projects", Some(&["MINGW64:/d/projects"]))), Reading::Following("MINGW64:/d/projects".into()));
        assert_eq!(read_front(&reading("Git Bash", Some(&["Git Bash"]))), Reading::Following("Git Bash".into()));
    }

    #[test]
    fn every_abstention_names_itself_and_no_two_share_a_name() {
        // The reasons go to the decision log, where a check that is structurally
        // silent must be distinguishable from one that looked and found nothing.
        let reasons: Vec<&str> = [
            reading("", Some(&["a"])),
            reading("a", None),
            reading("a", Some(&[])),
            reading("a", Some(&["x", "y"])),
        ]
        .iter()
        .map(|r| match read_front(r) {
            Reading::Unknown { reason, .. } => reason,
            other => panic!("expected an abstention, got {other:?}"),
        })
        .collect();
        let mut sorted = reasons.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), reasons.len(), "two abstentions share a log reason: {reasons:?}");
    }
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
