//! Terminal adapters: what the dashboard asks a terminal, and how each one
//! answers.
//!
//! One adapter per terminal, because the answers are terminal-specific and always
//! will be — agterm on macOS, Windows Terminal plus the Windows console on
//! Windows. An adapter's whole job is to turn whatever its terminal exposes into
//! this module's vocabulary; everything downstream is generic and names no
//! terminal at all. They answer from entirely different places: agterm from a
//! control socket and a state file, Windows Terminal from a window title, a
//! console object and UI Automation, and agwinterm from a control pipe and a
//! state file.
//!
//! Five questions, five callers:
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
//! - **What should this session's context line say?**
//!   [`TerminalAdapter::can_label`] says whether a terminal has a per-session
//!   context line at all, [`TerminalAdapter::label_targets`] lists each session
//!   with its title and what its context reads now, and
//!   [`TerminalAdapter::write_label`] writes one. `crate::terminals::labels`
//!   consumes them for a terminal whose sessions carry a field of their own beside
//!   the console title, writing a row's task there, decided from the live reading.
//!
//! They are different axes — the first is about a human, the second about a
//! screen, the third about the gap between what a screen shows and what is behind
//! it, the fourth and fifth about where and how this dashboard's own writing goes
//! — and they share the seam because they share all of its vocabulary.
//!
//! **A new capability goes here too, not beside here.** Every terminal-specific
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

use std::collections::HashMap;

/// The agterm adapter. Compiled for tests everywhere too, so its readings and
/// its diff run on every platform's test build.
#[cfg(any(target_os = "macos", test))]
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
/// The consumer of [`TerminalAdapter::label_targets`] and
/// [`TerminalAdapter::write_label`]. Names no terminal; it starts only where an
/// adapter exists and answers [`TerminalAdapter::can_label`].
pub mod labels;
/// Several adapters answering as one, for a platform where more than one terminal
/// can host a session. Only Windows builds one, so it is compiled there and for
/// tests.
#[cfg(any(target_os = "windows", test))]
pub mod composite;
/// agwinterm, a terminal whose sidebar names each session, reached over its
/// control pipe.
#[cfg(target_os = "windows")]
pub mod agwinterm;
/// agwinterm's control protocol as pure functions: request lines, reply and tree
/// parsing, and the text rules its write verbs enforce. Compiled for tests on
/// every platform so the parsing is exercised on a Mac too.
#[cfg(any(target_os = "windows", test))]
pub mod agwinterm_wire;
/// agwinterm's per-window state file as pure functions: which session a window
/// has selected and when the user left one. Compiled for tests everywhere, like
/// the wire module.
#[cfg(any(target_os = "windows", test))]
pub mod agwinterm_state;
/// The directory watch every adapter whose terminal saves its selection to a
/// file shares. Compiled where such an adapter exists.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod snapshot_watch;
/// Per-window bookkeeping over a directory of window files, shared by the same
/// adapters. Compiled for tests everywhere, so its rules run on every platform.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
pub mod window_files;

/// What agterm can tell the person verdict, as pure readings over its control
/// answers, plus the macOS calls that ask which application is in front.
/// Compiled for tests everywhere so the readings are exercised on Windows too.
#[cfg(any(target_os = "macos", test))]
pub mod agterm_facts;
/// agterm's `session context` protocol as pure functions: the argv it takes, the
/// targets its tree reports, and the release that first served the verb.
/// Compiled for tests everywhere, like the agwinterm wire module.
#[cfg(any(target_os = "macos", test))]
pub mod agterm_wire;

/// How close to a session switch the last input must be for the switch to be
/// attributable to a person. Measured on Windows Terminal, a human tab switch
/// lands at under 50 ms from its click or keystroke, so this is loose by two
/// orders of magnitude and still refuses a script switching while nobody types.
pub const SWITCH_INPUT_WINDOW_MS: i64 = 2_000;

/// How long a surface must already have held the foreground when a switch in it
/// is made for the switch to have been made by someone looking at it. One click
/// on a background window's tab or sidebar both activates the window and selects
/// a session, and the session selected before that click was never in front of
/// anyone.
///
/// **That is true only of a window that was covered**, and the rule refuses the
/// uncovered case too, as an accepted loss. A terminal window that was visible
/// without the focus — on a second monitor, beside a browser, or behind a click
/// on this dashboard's own widget — was being read, and the click straight onto
/// its next tab is a real departure from the one on screen, refused here as
/// `activated_by_switch`. Telling the two apart needs the window's occlusion
/// *before* the click, and neither platform reports that after the fact: the
/// foreground event arrives once the window is already on top, and a sampled
/// occlusion record has the blind spot of every sampled level. What is lost is
/// that first click alone: once the window has held the focus this long, its
/// switches and the input made in it are credited as usual.
pub const ACTIVATION_MS: i64 = 500;

/// How long after a selection began the click or key that made it can still be
/// landing. A key or button is released after the switch it made, so input this
/// close to the start of a selection is the arrival rather than reading, and an
/// arrival marks nothing.
pub const SWITCH_RELEASE_MS: i64 = 300;

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

/// When the selection on each surface began, noted by every source that sees it.
///
/// A terminal's watch and its poll both see selections, at different instants
/// and in no fixed order, so the answer is kept in one place they both write.
/// `same` decides whether two readings are one selection: agterm compares its
/// own session ids, Windows Terminal compares whole tab titles.
///
/// **Every instant noted is no earlier than the selection it reports**, which is
/// what makes the answer safe to compare input against: a selection's start
/// placed late refuses more input as the arrival, never less. Two things hold
/// that up. `same` must err toward "different": one that called two selections
/// one would keep the first one's start for the second, earlier than the truth,
/// which is why Windows Terminal does not use `terminal_title::same_row` here,
/// the comparator its departures use, since that one errs the other way. And a
/// selection that began and ended between two readings is not seen, so where
/// only a poll is running the start of the selection after it is placed too
/// early, at the first reading that showed it rather than at the return.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub struct SelectionClock<K> {
    by_surface: HashMap<K, (String, i64)>,
}

impl<K> Default for SelectionClock<K> {
    fn default() -> Self {
        Self { by_surface: HashMap::new() }
    }
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
impl<K: Eq + std::hash::Hash> SelectionClock<K> {

    /// Note that `surface` shows `selection` as of `at_ms`, and answer since when
    /// it has. A reading of the selection already held keeps its start, even
    /// when a slower source reports it with an earlier instant; anything else
    /// starts a new selection at `at_ms`.
    pub fn note(&mut self, surface: K, selection: &str, same: impl Fn(&str, &str) -> bool, at_ms: i64) -> i64 {
        let since = self.by_surface.get(&surface).filter(|(held, _)| same(held, selection)).map_or(at_ms, |&(_, since)| since);
        self.by_surface.insert(surface, (selection.to_string(), since));
        since
    }

    /// Forget every surface `keep` refuses, so a closed window's handle reused
    /// by a new one starts from nothing. Windows reuses window handles; agterm's
    /// window ids are never reused, so only the Windows adapter calls this.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.by_surface.retain(|k, _| keep(k));
    }
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

/// What the user was observed doing, with the facts the person verdict needs
/// about it.
///
/// Every enum from here to [`person_verdict`] allows `dead_code` because this is
/// the seam's vocabulary, not one terminal's: each terminal constructs only the
/// states it can observe — agwinterm and agterm report a covered session and
/// Windows Terminal never does, only agterm a program other than a shell — and on
/// a platform whose adapter is not written,
/// Linux, where [`for_platform`] answers `None`, nothing constructs any of them.
/// That is the expected state rather than a defect; deleting a variant to
/// silence it would delete a state the verdict has to judge for another
/// terminal.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationKind {
    /// The user **left** this session's tab, having been in it. The primary
    /// signal, because leaving is the moment you are done with what was on
    /// screen — whereas arriving proves only that you got there, and reading
    /// itself produces nothing at all to observe.
    Departed(Switch),
    /// The user produced input while this session was the one on screen. Weaker
    /// and secondary: it cannot see a silent read, and it is here for the case
    /// the user reads a finished answer and then types the next prompt without
    /// ever switching away. [`Observation::at_ms`] is the input's instant.
    Input(InputFacts),
}

/// The switch a departure was seen by.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    /// The terminal reported the switch as an edge, so it can be placed in time
    /// and asked about.
    ///
    /// **A switch is placed between two bounds, and each rule reads the one
    /// that errs its own safe way.** The earliest it can have been is
    /// [`Observation::at_ms`]; the latest is `latest_ms`. The activation rule
    /// refuses a switch made too soon after its surface came forward, so it
    /// reads the earliest bound: a switch placed early refuses more. The input
    /// rule refuses a switch with no input near it, so it reads the latest: input
    /// compared against a late placement is refused as too old more often. Read
    /// the other way round, a report placed late would pass the activation rule
    /// for the very click that brought the surface forward.
    Placed {
        /// The latest the switch can have been, where the terminal's report puts
        /// it.
        latest_ms: i64,
        /// Whether the surface the switch happened in was in front of the user.
        front: Front,
        /// Since when that surface had held the foreground.
        front_since: Since,
        /// The last input the terminal or the desktop saw, at or after the
        /// switch's report, as an absolute instant.
        ///
        /// It and `front` are read after the switch, and only describe it while
        /// the read follows the switch closely: input made after the switch
        /// would otherwise pass for input at it. An adapter that reads them
        /// later than its terminal's normal reporting delay reports both unknown;
        /// see [`facts_at_switch`].
        last_input: LastInput,
    },
    /// The terminal was sampled, and two samples showed different selections:
    /// the switch happened somewhere in between, and who made it is not known.
    Unplaced,
}

/// The facts an input observation is judged by.
///
/// The input instant is the desktop's last input, which says nothing about where
/// it went: it is this surface's only when the surface has held the foreground
/// from before the input until now. Every terminal reports it that way. A
/// terminal's own idle clock would say which window took the input, but agterm's,
/// the one such clock, counts something nobody has measured, and a window behind
/// another application still receives scroll and hover events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputFacts {
    /// Whether the surface is in front of the user now.
    pub front: Front,
    /// Since when the surface has held the foreground.
    pub front_since: Since,
    /// Whether the session named is still the surface's live selection.
    pub selection: Selection,
    /// Since when the session has been the surface's selection, no earlier than
    /// the truth: see [`SelectionClock`].
    pub selected_since: Since,
    /// Whether something is drawn over the session that takes its keystrokes.
    pub cover: Cover,
}

/// Whether a surface is in front of the user.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Front {
    Yes,
    /// Another program, or another window of this terminal, is in front.
    No,
    /// The terminal or the desktop could not be asked. The reason goes to the
    /// log.
    Unknown(&'static str),
}

/// Since when something has held, as an absolute instant **no earlier than the
/// truth**. Each rule that reads one refuses more when it is late, so an adapter
/// that knows only a window must report its end. agterm with several windows
/// open is the one accepted exception, since macOS reports no window
/// activation; see [`agterm_facts`].
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Since {
    At(i64),
    /// Nothing recorded when it began.
    Unrecorded,
}

/// The last input seen, as an absolute instant.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastInput {
    At(i64),
    Unknown,
}

/// Whether the session an input observation names is the surface's live
/// selection, which a terminal whose selection is read from a saved file has to
/// ask separately.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Live,
    /// The terminal says another session is selected now: what the observation
    /// was built from is behind the screen.
    Lagging,
    Unknown(&'static str),
}

/// Whether something is drawn over the session that takes its keystrokes, such
/// as an overlay terminal running over it.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cover {
    Clear,
    Covered,
    Unknown,
}

/// What runs in the session.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occupant {
    /// The agent, or something other than an idle shell where the terminal
    /// cannot tell the agent from another child of the shell.
    Agent,
    /// An idle shell, and no agent behind it.
    Shell,
    /// A program the terminal names, which is not the agent.
    Other,
    /// The terminal does not say. The reason goes to the log.
    Unknown(&'static str),
}

/// Where an observation's [`TerminalSession::title`] comes from.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming {
    /// The session's own title: the console or tty title, which this dashboard
    /// writes only onto the agent a row describes. An adapter reports this only
    /// where it read the title off the session itself, or off the program
    /// running in it, as agwinterm reports a pane's.
    OwnTitle,
    /// The name a terminal displays for the session, not confirmed to be the
    /// session's own title. A displayed name can stop following the session —
    /// a tab renamed by hand keeps the name it was showing, whatever the console
    /// behind it now holds — so a plain shell can carry the agent's name.
    DisplayedName,
}

/// How `attention::resolve_row` named a row from an observation's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedBy {
    Title,
    /// The working directory, the fallback when no title names a row.
    Directory,
    /// A title wearing `terminal_title::REMOTE_BADGE`, which named a **synced**
    /// row: the tab is a transport — an SSH client, a tmux attach — onto a
    /// session running on another machine, and the status it carries was
    /// written there.
    ///
    /// It is a third variant rather than a flag beside [`Title`](Self::Title)
    /// because it answers the occupant rule differently, and that difference is
    /// the whole reason it exists. What runs in such a tab is the transport, so
    /// the terminal reports [`Occupant::Other`] — correctly — and the ordinary
    /// rule refuses it. The badge is what distinguishes "another program is in
    /// this tab, so the title may be a leftover" from "the agent is at the far
    /// end of this program", and only a title the far machine's dashboard is
    /// still writing can carry it.
    RemoteTitle,
}

/// One thing a terminal observed, at a known instant, with the facts the person
/// verdict needs about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The slug of the terminal that observed it, the [`TerminalAdapter::name`]
    /// of the adapter that made this rather than of whatever forwarded it. On a
    /// platform where several terminals answer as one, the decision log needs it
    /// to say which sensor named nothing.
    pub terminal: &'static str,
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
    /// What runs in the session, for a departure the one left and for input the
    /// one on screen.
    pub occupant: Occupant,
    pub naming: Naming,
}

/// Why an observation was not credited to a person reading its session. Logged
/// by `crate::attention` as the `outcome` of a `decision = "attention_poll"` line,
/// one vocabulary for every terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    SwitchUnplaced,
    NotInFront,
    FrontUnknown(&'static str),
    ForegroundUnrecorded,
    ActivatedBySwitch,
    InputUnknown,
    NoRecentInput,
    InputBeforeFront,
    ForegroundInput,
    SelectionLagging,
    SelectionUnknown(&'static str),
    SelectionUnplaced,
    SwitchInput,
    Covered,
    CoverUnknown,
    BareShell,
    NotTheAgent,
    OccupantUnknown(&'static str),
}

impl Refusal {
    pub fn slug(&self) -> &'static str {
        match self {
            Refusal::SwitchUnplaced => "switch_unplaced",
            Refusal::NotInFront => "not_in_front",
            Refusal::FrontUnknown(_) => "front_unknown",
            Refusal::ForegroundUnrecorded => "foreground_unrecorded",
            Refusal::ActivatedBySwitch => "activated_by_switch",
            Refusal::InputUnknown => "input_unknown",
            Refusal::NoRecentInput => "no_recent_input",
            Refusal::InputBeforeFront => "input_before_front",
            Refusal::ForegroundInput => "foreground_input",
            Refusal::SelectionLagging => "selection_lagging",
            Refusal::SelectionUnknown(_) => "selection_unknown",
            Refusal::SelectionUnplaced => "selection_unplaced",
            Refusal::SwitchInput => "switch_input",
            Refusal::Covered => "covered",
            Refusal::CoverUnknown => "cover_unknown",
            Refusal::BareShell => "bare_shell",
            Refusal::NotTheAgent => "not_the_agent",
            Refusal::OccupantUnknown(_) => "occupant_unknown",
        }
    }

    /// Why a fact was unknown, where the terminal said.
    pub fn detail(&self) -> Option<&'static str> {
        match self {
            Refusal::FrontUnknown(why) | Refusal::SelectionUnknown(why) | Refusal::OccupantUnknown(why) => Some(why),
            _ => None,
        }
    }
}

/// Whether an observation can be credited to a person reading the session it
/// names, or the first rule it fails. Pure, cross-terminal, and the whole
/// judgment: every terminal-specific fact was turned into this vocabulary by its
/// adapter, and `crate::attention` applies this to every observation before it
/// marks anything read.
///
/// **An unknown fact refuses**, because the one direction this must never fail
/// in is marking a row read that nobody read; a refusal only leaves a row
/// showing, which the next observation corrects. The one rule whose unknown is
/// not refused is the occupant rule out of its scope, below.
///
/// A departure must be a person's switch:
///
/// - **Placed** (`switch_unplaced`): a switch found by comparing two samples
///   happened at an unknown instant, so nothing below can be asked about it.
///   A poll that finds switches finds them this way, so without a watch no
///   departure is credited anywhere, and only input remains.
/// - **In front** (`not_in_front`, `front_unknown`): the surface it happened in
///   was in front. A pipe or CLI select in a window behind another is a script.
/// - **Held before** (`foreground_unrecorded`, `activated_by_switch`): the
///   surface had held the foreground for more than [`ACTIVATION_MS`] before the
///   *earliest* the switch can have been, so the switch was not the click that
///   brought it forward, whose previous selection was behind another window.
/// - **Input at the switch** (`input_unknown`, `no_recent_input`): the last
///   input is no more than [`SWITCH_INPUT_WINDOW_MS`] older than the *latest*
///   the switch can have been, since a switch is itself a click or a keystroke.
///
/// Input must reach the session on screen:
///
/// - **Reach** (`not_in_front`, `front_unknown`, `foreground_unrecorded`,
///   `input_before_front`): desktop-wide input counts only when the surface
///   holds the foreground now and has held it since before the input.
/// - **After the surface came forward** (`foreground_input`): the input came
///   more than [`SWITCH_RELEASE_MS`] after the surface came to the foreground,
///   since the click that brings a window forward is released after the
///   activation it made and is the newest input then, an arrival rather than
///   reading. A foreground start recorded late refuses more here, never less.
/// - **Live** (`selection_lagging`, `selection_unknown`): the terminal confirms
///   the session is still its selection.
/// - **After the arrival** (`selection_unplaced`, `switch_input`): the input
///   came more than [`SWITCH_RELEASE_MS`] after the session was selected.
/// - **Uncovered** (`covered`, `cover_unknown`): nothing drawn over the session
///   takes its keystrokes.
///
/// Both must be the agent (`bare_shell`, `not_the_agent`, `occupant_unknown`).
/// **A session a terminal knows is not the agent is refused whatever named it,
/// with one exception** — a title wearing `terminal_title::REMOTE_BADGE`
/// ([`NamedBy::RemoteTitle`]), where the program in the tab is the transport to
/// an agent on another machine rather than something left in front of its
/// title. A shell is refused even there, which is what keeps the rule's reach:
/// once the transport exits, the tab drops back to a local prompt still
/// carrying the last title the far machine sent.
/// A shell left in a tab after its agent exited keeps the agent's last title
/// until something rewrites it, and the shells these terminals run do not, so a
/// session's own title is no proof the agent is still in it.
///
/// **An occupant nobody could report is asked only where it can matter.** The
/// rule exists because a session that is not the agent can still name the
/// agent's row: through a name this dashboard did not write onto the agent
/// itself — a working directory, which `resolve_row` falls back to — or through
/// a title the agent left behind. The first keeps the rule in scope, and so does
/// a name a terminal merely *displays*, since a displayed name can outlive what
/// it was written for ([`Naming::DisplayedName`]). A row named by the session's
/// own title is out of scope for an unknown occupant, which is what keeps Windows
/// Terminal working, whose panes say nothing about what runs in them, and
/// agwinterm's WSL sessions, whose root is no shell agwinterm recognizes. The
/// title left behind is
/// what that scope leaves, and `terminal_title::sync` closes it: when a row is
/// removed its last title is blanked on the console or tty it was written to,
/// reached through the session's surviving processes rather than the agent's, and
/// retried until it lands. What the scope still trusts is a title outliving its
/// agent while another session keeps the row alive, two sessions in one
/// directory, where nothing is removed and so nothing is blanked. A Windows
/// Terminal departure from such a tab is refused, as a [`Naming::DisplayedName`],
/// once the row's title has moved since the tab's caption was read; input typed
/// into it while it still carries the row's current title is credited.
pub fn person_verdict(o: &Observation, named_by: NamedBy) -> Result<(), Refusal> {
    match o.kind {
        ObservationKind::Departed(switch) => person_switched(o.at_ms, switch)?,
        ObservationKind::Input(facts) => input_reached(o.at_ms, facts)?,
    }
    match (o.occupant, named_by, o.naming) {
        (Occupant::Agent, ..) => Ok(()),
        (Occupant::Shell, ..) => Err(Refusal::BareShell),
        // A transport onto an agent on another machine. The occupant really is
        // another program — an `ssh`, a `mosh`, a tmux client — and the rule
        // below is right to refuse that everywhere else, because a program left
        // in a tab keeps the title of the agent that was there before it. What
        // makes this case different is not the program but the title: only the
        // far machine's dashboard writes the badge, it writes it onto a session
        // it is still tracking, and `attention::resolve_row` admits it only
        // where a live synced row answers to the name. So the agent is at the
        // far end of this program rather than gone from in front of it.
        //
        // `Shell` above still refuses, and that ordering is the guard: once the
        // transport exits, the tab drops back to a local prompt while the title
        // it was last sent stays on the tab, which is exactly the leftover the
        // rule exists for.
        (Occupant::Other, NamedBy::RemoteTitle, _) => Ok(()),
        (Occupant::Other, ..) => Err(Refusal::NotTheAgent),
        (Occupant::Unknown(_), NamedBy::Title, Naming::OwnTitle) => Ok(()),
        (Occupant::Unknown(why), ..) => Err(Refusal::OccupantUnknown(why)),
    }
}

/// The departure half of [`person_verdict`], for a switch no earlier than
/// `earliest_ms`.
fn person_switched(earliest_ms: i64, switch: Switch) -> Result<(), Refusal> {
    let Switch::Placed { latest_ms, front, front_since, last_input } = switch else { return Err(Refusal::SwitchUnplaced) };
    in_front(front)?;
    match front_since {
        Since::Unrecorded => return Err(Refusal::ForegroundUnrecorded),
        Since::At(since) if earliest_ms <= since + ACTIVATION_MS => return Err(Refusal::ActivatedBySwitch),
        Since::At(_) => {}
    }
    match last_input {
        LastInput::Unknown => Err(Refusal::InputUnknown),
        LastInput::At(input) if input < latest_ms - SWITCH_INPUT_WINDOW_MS => Err(Refusal::NoRecentInput),
        LastInput::At(_) => Ok(()),
    }
}

/// The input half of [`person_verdict`], for input at `at_ms`.
fn input_reached(at_ms: i64, facts: InputFacts) -> Result<(), Refusal> {
    in_front(facts.front)?;
    match facts.front_since {
        Since::Unrecorded => return Err(Refusal::ForegroundUnrecorded),
        Since::At(since) if at_ms < since => return Err(Refusal::InputBeforeFront),
        Since::At(since) if at_ms <= since + SWITCH_RELEASE_MS => return Err(Refusal::ForegroundInput),
        Since::At(_) => {}
    }
    match facts.selection {
        Selection::Live => {}
        Selection::Lagging => return Err(Refusal::SelectionLagging),
        Selection::Unknown(why) => return Err(Refusal::SelectionUnknown(why)),
    }
    match facts.selected_since {
        Since::Unrecorded => return Err(Refusal::SelectionUnplaced),
        Since::At(since) if at_ms <= since + SWITCH_RELEASE_MS => return Err(Refusal::SwitchInput),
        Since::At(_) => {}
    }
    match facts.cover {
        Cover::Clear => Ok(()),
        Cover::Covered => Err(Refusal::Covered),
        Cover::Unknown => Err(Refusal::CoverUnknown),
    }
}

fn in_front(front: Front) -> Result<(), Refusal> {
    match front {
        Front::Yes => Ok(()),
        Front::No => Err(Refusal::NotInFront),
        Front::Unknown(why) => Err(Refusal::FrontUnknown(why)),
    }
}

/// The foreground and input facts read at `read_ms` about a switch placed no
/// later than `latest_ms`, or both unknown when the read came too late to
/// describe the switch.
///
/// For an adapter that learns of a switch from a file its terminal saves, and
/// reads the facts when the save arrives rather than at the switch. Read
/// promptly, they are the switch's own: the window still in front and the input
/// that made it. Read later than `allowance_ms`, the terminal's normal delay
/// between a switch and the read of its save, they describe whatever happened
/// since — input made after the switch would pass for input at it, and the
/// verdict's input rule would become "any input since". The allowance is the
/// adapter's, since only it knows its terminal's delay.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub fn facts_at_switch(front: Front, last_input: LastInput, read_ms: i64, latest_ms: i64, allowance_ms: i64) -> (Front, LastInput) {
    if read_ms - latest_ms > allowance_ms {
        (Front::Unknown("read_after_switch"), LastInput::Unknown)
    } else {
        (front, last_input)
    }
}

/// A terminal this dashboard can ask about.
///
/// Five questions, all answered in the vocabulary above. *What did the user do*
/// ([`poll`](TerminalAdapter::poll) / [`watch`](TerminalAdapter::watch)), *what
/// are you showing* ([`sessions`](TerminalAdapter::sessions)), *what are you
/// displaying versus what is really in there*
/// ([`front_readings`](TerminalAdapter::front_readings)), *where did that
/// write land* ([`attached_surface`](TerminalAdapter::attached_surface)), and
/// *what should this session's context line say*
/// ([`label_targets`](TerminalAdapter::label_targets) /
/// [`write_label`](TerminalAdapter::write_label)). They are different axes — a
/// human, a screen, the gap between a screen and what is behind it, and where and
/// how this dashboard's own writing goes — but they share the seam because they
/// share its whole vocabulary: a session named by `cwd` and `title`, which is all
/// any caller needs and all any terminal can be relied on to have.
///
/// `name`, `sessions` and `poll` are required. Every other method has a default
/// that declines to answer, so an adapter implements what its terminal can
/// actually tell it and each caller learns the difference between a `None` and a
/// finding.
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

    /// Whether this terminal gives its sessions a context line of their own at
    /// all.
    ///
    /// A fact about the terminal rather than about this moment, so it is asked
    /// separately from [`label_targets`](Self::label_targets), whose `None` also
    /// means "could not look just now" and is retried. `labels`' worker starts
    /// only where this answers `Some(true)`, so a terminal with no context line
    /// is not asked every few seconds for the life of the process.
    ///
    /// `None` is the third answer, and it is why this is not a bool: agterm's
    /// answer needs its control socket, and at login the dashboard and the
    /// terminal start in no fixed order, so "could not ask yet" has to be
    /// distinguishable from "this terminal has no context line". The caller
    /// retries a `None` and takes a `Some` as settled for the life of the
    /// process. A terminal that knows statically answers `Some` and blocks
    /// nothing.
    fn can_label(&self) -> Option<bool> {
        Some(false)
    }

    /// Every session this terminal can label, with its title and the context it
    /// shows right now, or `None` when the terminal could not be asked.
    ///
    /// The line [`sessions`](Self::sessions) draws: `Some(vec![])` means the
    /// terminal answered and has nothing to label, which includes "not running";
    /// `None` means it could not look, and the caller retries rather than reading
    /// silence as an empty terminal.
    ///
    /// Each reading is the terminal's own, never what this dashboard remembers
    /// writing, so a write is decided against what is really there: a context the
    /// user changed, or one the terminal lost, shows up here.
    ///
    /// **May block**, for as long as a cross-process read takes. It is called only
    /// from `labels`' worker thread, never from an emit.
    fn label_targets(&self) -> Option<Vec<LabelTarget>> {
        None
    }

    /// Write one session's context line. `key` is a [`LabelTarget::key`] this
    /// adapter minted.
    ///
    /// **May block**, for as long as the terminal takes to apply the write, and
    /// is called only from `labels`' worker thread. The default refuses, because a
    /// terminal that lists no targets is never asked.
    fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
        let _ = (key, write);
        Err("this terminal cannot be labelled".to_string())
    }
}

/// One labellable session, as [`TerminalAdapter::label_targets`] reports it.
///
/// `dead_code` is allowed only where no adapter labels, for the reason
/// [`ObservationKind`]'s is: agwinterm and agterm both construct these, so the
/// platforms that do are Windows and macOS.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelTarget {
    /// Which session this is, opaque to every caller, with the contract
    /// [`FrontReading::surface`] has: compare it, pass it back to
    /// [`TerminalAdapter::write_label`], and do nothing else with it.
    pub key: String,
    /// The program title of the session's focused pane, which for an agent is the
    /// console title this dashboard writes, or `None` where it has none. What
    /// joins the session to a row.
    pub title: Option<String>,
    /// The session's context line, or `None` where none is set.
    pub context: Option<String>,
    /// The longest context the terminal accepts, in the unit that terminal
    /// enforces, which `labels` fits a row's text to.
    pub budget: LabelBudget,
}

/// How long a context a terminal accepts, in the unit it measures.
///
/// The unit is on the seam because the two terminals do not agree on it *or* on
/// what happens past it, and the difference is not cosmetic. agwinterm's 200 is
/// UTF-16 code units and advisory — a longer write is accepted and shown cut.
/// agterm's 256 is UTF-8 bytes and enforced: the write is refused outright with
/// `context must be at most 256 UTF-8 bytes`, leaving the previous value
/// standing. So a budget stated in the wrong unit is not a cosmetic slip —
/// 200 UTF-16 units of Cyrillic or emoji is up to 800 UTF-8 bytes, and
/// `labels::pass` abandons the rest of its pass on the first `Err`, so one
/// over-budget row would starve every row planned after it on a 10s retry loop.
/// The adapter states the unit; the pure fitter in `labels` obeys it.
/// `dead_code` is allowed unconditionally here, which no sibling type needs: a
/// variant is constructed by whichever adapter enforces that unit, so the lib
/// build constructs exactly one of them on each platform — `Utf8Bytes` on macOS
/// and `Utf16` on Windows — and a cfg-conditional allow would therefore be wrong
/// on both. The vocabulary is deliberately wider than any one platform, which is
/// the whole point of stating the unit on the seam.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelBudget {
    Utf16(usize),
    Utf8Bytes(usize),
}

/// One write to a session's context line. A [`LabelWrite::Context`] is never
/// longer than its target's [`LabelTarget::budget`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelWrite {
    Context(String),
    ClearContext,
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
        // `mut` for the Windows push below; nothing mutates it elsewhere, and the
        // list grows with the next adapter.
        #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
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

#[cfg(test)]
pub(crate) mod verdict_tests {
    use super::*;

    /// A departure a person made in Windows Terminal, as its watch reports one:
    /// in front for a minute, input at the switch, nothing said about the pane.
    pub(crate) fn departure() -> Observation {
        let switch = Switch::Placed { latest_ms: 60_000, front: Front::Yes, front_since: Since::At(0), last_input: LastInput::At(59_990) };
        Observation { terminal: "test", session: TerminalSession { cwd: None, title: Some("🟢 dash".into()) }, at_ms: 59_750, kind: ObservationKind::Departed(switch), occupant: Occupant::Unknown("not_reported"), naming: Naming::OwnTitle }
    }

    /// Input a person made into a session they had been on for a minute, with the
    /// desktop clock and the foreground behind it.
    pub(crate) fn input() -> Observation {
        let facts = InputFacts { front: Front::Yes, front_since: Since::At(0), selection: Selection::Live, selected_since: Since::At(0), cover: Cover::Clear };
        Observation { kind: ObservationKind::Input(facts), at_ms: 60_000, ..departure() }
    }

    fn with_switch(o: Observation, edit: impl Fn(&mut i64, &mut Front, &mut Since, &mut LastInput)) -> Observation {
        let ObservationKind::Departed(Switch::Placed { mut latest_ms, mut front, mut front_since, mut last_input }) = o.kind else { unreachable!("a placed departure") };
        edit(&mut latest_ms, &mut front, &mut front_since, &mut last_input);
        Observation { kind: ObservationKind::Departed(Switch::Placed { latest_ms, front, front_since, last_input }), ..o }
    }

    fn with_input(o: Observation, edit: impl Fn(&mut InputFacts)) -> Observation {
        let ObservationKind::Input(mut facts) = o.kind else { unreachable!("an input") };
        edit(&mut facts);
        Observation { kind: ObservationKind::Input(facts), ..o }
    }

    fn occupied(o: Observation, occupant: Occupant, naming: Naming) -> Observation {
        Observation { occupant, naming, ..o }
    }

    #[test]
    fn every_departure_rule_refuses_its_own_case_and_only_that() {
        let cases: Vec<(&str, Observation, Result<(), Refusal>)> = vec![
            ("a person's switch", departure(), Ok(())),
            ("sampled, not seen as an edge", Observation { kind: ObservationKind::Departed(Switch::Unplaced), ..departure() }, Err(Refusal::SwitchUnplaced)),
            ("a window behind another", with_switch(departure(), |_, f, _, _| *f = Front::No), Err(Refusal::NotInFront)),
            ("the front could not be asked", with_switch(departure(), |_, f, _, _| *f = Front::Unknown("pipe_unanswered")), Err(Refusal::FrontUnknown("pipe_unanswered"))),
            ("no record of the window coming forward", with_switch(departure(), |_, _, s, _| *s = Since::Unrecorded), Err(Refusal::ForegroundUnrecorded)),
            ("the click that brought it forward", with_switch(departure(), |_, _, s, _| *s = Since::At(59_750 - ACTIVATION_MS)), Err(Refusal::ActivatedBySwitch)),
            ("held just long enough before the earliest the switch can have been", with_switch(departure(), |_, _, s, _| *s = Since::At(59_750 - ACTIVATION_MS - 1)), Ok(())),
            ("no input clock", with_switch(departure(), |_, _, _, i| *i = LastInput::Unknown), Err(Refusal::InputUnknown)),
            ("nobody touched anything near the switch", with_switch(departure(), |_, _, _, i| *i = LastInput::At(60_000 - SWITCH_INPUT_WINDOW_MS - 1)), Err(Refusal::NoRecentInput)),
            ("input at the edge of the window", with_switch(departure(), |_, _, _, i| *i = LastInput::At(60_000 - SWITCH_INPUT_WINDOW_MS)), Ok(())),
        ];
        for (name, o, want) in cases {
            assert_eq!(person_verdict(&o, NamedBy::Title), want, "{name}");
        }
    }

    #[test]
    fn every_input_rule_refuses_its_own_case_and_only_that() {
        let desktop = |front, since| move |f: &mut InputFacts| (f.front, f.front_since) = (front, since);
        let cases: Vec<(&str, Observation, Result<(), Refusal>)> = vec![
            ("input to the session on screen", input(), Ok(())),
            ("typing in another program", with_input(input(), desktop(Front::No, Since::At(0))), Err(Refusal::NotInFront)),
            ("the front could not be asked", with_input(input(), desktop(Front::Unknown("no_window"), Since::At(0))), Err(Refusal::FrontUnknown("no_window"))),
            ("in front, but since when is not known", with_input(input(), desktop(Front::Yes, Since::Unrecorded)), Err(Refusal::ForegroundUnrecorded)),
            ("typed before the surface came forward", with_input(input(), desktop(Front::Yes, Since::At(60_001))), Err(Refusal::InputBeforeFront)),
            ("the click that brought the surface forward", with_input(input(), desktop(Front::Yes, Since::At(60_000))), Err(Refusal::ForegroundInput)),
            ("the release of that click", with_input(input(), desktop(Front::Yes, Since::At(60_000 - SWITCH_RELEASE_MS))), Err(Refusal::ForegroundInput)),
            ("just after the activation", with_input(input(), desktop(Front::Yes, Since::At(60_000 - SWITCH_RELEASE_MS - 1))), Ok(())),
            ("the file is behind the screen", with_input(input(), |f| f.selection = Selection::Lagging), Err(Refusal::SelectionLagging)),
            ("the terminal did not confirm the selection", with_input(input(), |f| f.selection = Selection::Unknown("tree_unanswered")), Err(Refusal::SelectionUnknown("tree_unanswered"))),
            ("no record of the selection beginning", with_input(input(), |f| f.selected_since = Since::Unrecorded), Err(Refusal::SelectionUnplaced)),
            ("the click that arrived", with_input(input(), |f| f.selected_since = Since::At(60_000 - SWITCH_RELEASE_MS)), Err(Refusal::SwitchInput)),
            ("just after the arrival", with_input(input(), |f| f.selected_since = Since::At(60_000 - SWITCH_RELEASE_MS - 1)), Ok(())),
            ("an overlay takes the keystrokes", with_input(input(), |f| f.cover = Cover::Covered), Err(Refusal::Covered)),
            ("the terminal did not say what is drawn over it", with_input(input(), |f| f.cover = Cover::Unknown), Err(Refusal::CoverUnknown)),
        ];
        for (name, o, want) in cases {
            assert_eq!(person_verdict(&o, NamedBy::Title), want, "{name}");
        }
    }

    #[test]
    fn a_foreground_start_recorded_late_refuses_the_click_that_brought_the_surface_forward() {
        // An activation is recorded when its notification reaches this process,
        // after the click that made it, and the input clock may be read before
        // the terminal answers. Either error puts the click before the recorded
        // start, which is refused rather than matched to an older stretch.
        let late = |since| with_input(input(), move |f| f.front_since = Since::At(since));
        assert_eq!(person_verdict(&late(60_000 + 150), NamedBy::Title), Err(Refusal::InputBeforeFront), "the activating click, recorded 150 ms late");
        assert_eq!(person_verdict(&late(60_000 - 100), NamedBy::Title), Err(Refusal::ForegroundInput), "recorded early enough to precede it, still the arrival");
    }

    #[test]
    fn a_switch_placed_late_is_judged_by_its_earliest_bound_for_activation_and_its_latest_for_input() {
        // A save that reached the disk long after the switch it carries: the
        // switch is no earlier than 1 000 and the report places it at 20 000.
        // The surface came forward at 900, by the click that made the switch.
        let late = Observation { at_ms: 1_000, ..with_switch(departure(), |at, _, s, i| (*at, *s, *i) = (20_000, Since::At(900), LastInput::At(19_900))) };
        assert_eq!(person_verdict(&late, NamedBy::Title), Err(Refusal::ActivatedBySwitch), "the report's late placement must not pass the activation rule");
        // The same bounds with the surface long in front: the input rule asks
        // about the latest placement, which refuses input older than it more.
        let held = with_switch(late.clone(), |_, _, s, _| *s = Since::At(-60_000));
        assert_eq!(person_verdict(&held, NamedBy::Title), Ok(()));
        assert_eq!(person_verdict(&with_switch(held, |_, _, _, i| *i = LastInput::At(2_000)), NamedBy::Title), Err(Refusal::NoRecentInput), "input near the earliest bound is too old for the latest");
    }

    #[test]
    fn facts_read_too_long_after_a_switch_are_unknown() {
        let read = |read_ms| facts_at_switch(Front::Yes, LastInput::At(9_990), read_ms, 10_000, 500);
        assert_eq!(read(10_500), (Front::Yes, LastInput::At(9_990)), "read within the terminal's normal delay");
        assert_eq!(read(10_501), (Front::Unknown("read_after_switch"), LastInput::Unknown), "later, input since the switch would pass for input at it");
    }

    #[test]
    fn the_occupant_matters_wherever_a_name_could_have_come_from_a_directory() {
        // In scope: a row named through the working directory, which a plain
        // shell in the project directory reaches, or a name a terminal only
        // displays.
        for (named_by, naming) in [(NamedBy::Directory, Naming::OwnTitle), (NamedBy::Title, Naming::DisplayedName), (NamedBy::Directory, Naming::DisplayedName)] {
            for o in [departure(), input()] {
                let case = |occupant| person_verdict(&occupied(o.clone(), occupant, naming), named_by);
                assert_eq!(case(Occupant::Agent), Ok(()), "{named_by:?} {naming:?}");
                assert_eq!(case(Occupant::Shell), Err(Refusal::BareShell), "{named_by:?} {naming:?}");
                assert_eq!(case(Occupant::Other), Err(Refusal::NotTheAgent), "{named_by:?} {naming:?}");
                assert_eq!(case(Occupant::Unknown("not_in_tree")), Err(Refusal::OccupantUnknown("not_in_tree")), "{named_by:?} {naming:?}");
            }
        }
    }

    #[test]
    fn an_unknown_occupant_cannot_matter_for_a_row_named_by_the_sessions_own_title() {
        // A title this dashboard wrote onto the agent's own console names the
        // agent's session, so an occupant nobody could report is not asked;
        // Windows Terminal's panes never say.
        for o in [departure(), input()] {
            for occupant in [Occupant::Agent, Occupant::Unknown("not_reported")] {
                assert_eq!(person_verdict(&occupied(o.clone(), occupant, Naming::OwnTitle), NamedBy::Title), Ok(()), "{occupant:?}");
            }
        }
    }

    #[test]
    fn a_known_non_agent_is_refused_even_under_its_own_title() {
        // A shell left in a tab after its agent exited keeps the agent's last
        // title: agterm reports `-zsh` in front of a tab still titled `🟢 dash`.
        // Crediting it would mark read a new `dash` row whose answer is in
        // another tab.
        for o in [departure(), input()] {
            assert_eq!(person_verdict(&occupied(o.clone(), Occupant::Shell, Naming::OwnTitle), NamedBy::Title), Err(Refusal::BareShell));
            assert_eq!(person_verdict(&occupied(o.clone(), Occupant::Other, Naming::OwnTitle), NamedBy::Title), Err(Refusal::NotTheAgent));
        }
    }

    #[test]
    fn a_transport_onto_another_machines_agent_is_credited_under_the_remote_badge() {
        // An SSH or tmux tab really is running another program — the rule above
        // is right about that and right to refuse it everywhere else. What the
        // badge adds is that the far machine's dashboard is still writing this
        // title, onto a session it is still tracking, so the agent is at the far
        // end of the transport rather than gone from in front of it.
        for o in [departure(), input()] {
            let via_transport = occupied(o.clone(), Occupant::Other, Naming::OwnTitle);
            assert_eq!(person_verdict(&via_transport, NamedBy::RemoteTitle), Ok(()));
            // The guard that keeps the original rule's reach: once the transport
            // exits, the tab drops to a local prompt while the title it was last
            // sent stays on it — the leftover that rule exists for.
            assert_eq!(person_verdict(&occupied(o.clone(), Occupant::Shell, Naming::OwnTitle), NamedBy::RemoteTitle), Err(Refusal::BareShell));
            // And a terminal that says nothing about what is in the tab cannot
            // establish the transport, so it stays refused.
            assert_eq!(
                person_verdict(&occupied(o.clone(), Occupant::Unknown("not_reported"), Naming::OwnTitle), NamedBy::RemoteTitle),
                Err(Refusal::OccupantUnknown("not_reported"))
            );
        }
    }

    #[test]
    fn a_person_rule_outranks_the_occupant_rule() {
        // A scripted switch on a bare shell is logged as the script, the first
        // thing wrong with it.
        let o = occupied(with_switch(departure(), |_, f, _, _| *f = Front::No), Occupant::Shell, Naming::OwnTitle);
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::NotInFront));
    }

    #[test]
    fn each_terminals_typical_facts_reach_the_expected_verdict() {
        // A switch any terminal's poll finds by comparing two samples is refused
        // whatever the terminal: who made it, at an unknown instant, cannot be
        // asked. Without a watch, no terminal's departures are credited.
        let poll_departure = Observation { kind: ObservationKind::Departed(Switch::Unplaced), ..departure() };
        assert_eq!(person_verdict(&poll_departure, NamedBy::Title), Err(Refusal::SwitchUnplaced), "any poll");
        // Windows Terminal: the watch knows the foreground and the input clock at
        // the event, and nothing about the pane, and the tab's name was read off
        // the pane's own console title.
        assert_eq!(person_verdict(&departure(), NamedBy::Title), Ok(()), "Windows Terminal watch");
        assert_eq!(person_verdict(&input(), NamedBy::Title), Ok(()), "Windows Terminal input");
        assert_eq!(person_verdict(&occupied(departure(), Occupant::Unknown("not_reported"), Naming::DisplayedName), NamedBy::Title), Err(Refusal::OccupantUnknown("not_reported")), "Windows Terminal tab whose name is not its pane's title");
        // agwinterm: the focused pane's program title, which for the agent is
        // the console title this dashboard wrote, forwarded through WSL and tmux.
        let agw = occupied(departure(), Occupant::Agent, Naming::OwnTitle);
        assert_eq!(person_verdict(&agw, NamedBy::Title), Ok(()), "agwinterm departure from the agent");
        assert_eq!(person_verdict(&occupied(agw.clone(), Occupant::Unknown("unrecognized_root"), Naming::OwnTitle), NamedBy::Title), Ok(()), "agwinterm WSL session, named by the agent's own title");
        assert_eq!(person_verdict(&occupied(agw, Occupant::Shell, Naming::OwnTitle), NamedBy::Title), Err(Refusal::BareShell), "agwinterm session whose agent exited, its title left behind");
        // agterm: its own title and a cwd beside it, with the agent in front.
        let agterm = occupied(input(), Occupant::Agent, Naming::OwnTitle);
        assert_eq!(person_verdict(&agterm, NamedBy::Title), Ok(()), "agterm input named by its title");
        assert_eq!(person_verdict(&agterm, NamedBy::Directory), Ok(()), "agterm input named by the directory, the agent in front");
        assert_eq!(person_verdict(&occupied(agterm.clone(), Occupant::Unknown("no_foreground"), Naming::OwnTitle), NamedBy::Directory), Err(Refusal::OccupantUnknown("no_foreground")), "agterm input named by the directory alone, nothing in front reported");
        assert_eq!(person_verdict(&occupied(agterm, Occupant::Shell, Naming::OwnTitle), NamedBy::Title), Err(Refusal::BareShell), "agterm tab whose agent exited, its title left behind");
    }

    #[test]
    fn every_refusal_has_its_own_slug() {
        let all = [
            Refusal::SwitchUnplaced,
            Refusal::NotInFront,
            Refusal::FrontUnknown("x"),
            Refusal::ForegroundUnrecorded,
            Refusal::ActivatedBySwitch,
            Refusal::InputUnknown,
            Refusal::NoRecentInput,
            Refusal::InputBeforeFront,
            Refusal::ForegroundInput,
            Refusal::SelectionLagging,
            Refusal::SelectionUnknown("x"),
            Refusal::SelectionUnplaced,
            Refusal::SwitchInput,
            Refusal::Covered,
            Refusal::CoverUnknown,
            Refusal::BareShell,
            Refusal::NotTheAgent,
            Refusal::OccupantUnknown("x"),
        ];
        let mut slugs: Vec<&str> = all.iter().map(Refusal::slug).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), all.len());
        assert_eq!(Refusal::OccupantUnknown("unrecognized_root").detail(), Some("unrecognized_root"));
        assert_eq!(Refusal::NotInFront.detail(), None);
    }

    #[test]
    fn a_selection_keeps_its_start_until_another_selection_replaces_it() {
        let mut clock = SelectionClock::default();
        let eq = |a: &str, b: &str| a == b;
        assert_eq!(clock.note(1, "S1", eq, 1_000), 1_000, "a first reading starts the selection");
        assert_eq!(clock.note(1, "S1", eq, 5_000), 1_000, "a later reading of the same one keeps its start");
        assert_eq!(clock.note(1, "S1", eq, 500), 1_000, "a slower source reporting it earlier does not move the start back");
        assert_eq!(clock.note(1, "S2", eq, 6_000), 6_000, "another selection starts afresh");
        assert_eq!(clock.note(2, "S2", eq, 7_000), 7_000, "surfaces are separate");
        clock.retain(|k| *k == 2);
        assert_eq!(clock.note(1, "S2", eq, 8_000), 8_000, "a forgotten surface starts from nothing");
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
    // A composite, because two terminals can host a session here and each knows
    // only its own: Windows Terminal answers restore and the stale check,
    // agwinterm answers labelling, and both answer attention. The composite keeps
    // Windows Terminal's slug, so the lines every existing consumer logs keep
    // theirs; an observation carries the slug of the terminal that made it.
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(composite::Composite::new(windows::NAME, vec![Box::new(windows::WindowsAdapter::new(app.clone())), Box::new(agwinterm::AgwintermAdapter::new())])))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = app;
        None
    }
}
