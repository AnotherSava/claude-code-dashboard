//! agwinterm's per-window state file, as pure functions: which session a window
//! has selected, when the user left one, what the session is called, and the
//! facts the person verdict judges that by.
//!
//! agwinterm saves each window to `%LOCALAPPDATA%\<app id>\windows\<window id>.json`
//! on every selection change (its `SetActive` always ends in a save), through one
//! writer thread for every file. The writer wakes on a save, lets the burst
//! settle for [`SETTLE_MS`], and writes the newest snapshot of each pending file
//! by renaming a temporary file over the old one. Unchanged bytes are not
//! rewritten, and a save whose rename fails is dropped until the next save comes.
//! The file is undocumented and carries no version field (its own source says
//! additive keys only), so a shape this module does not recognize is refused
//! rather than guessed at.
//!
//! The selection is the top-level `ActiveId`, and `Mru` is the recency order
//! with the most recent first. They disagree in exactly one situation: a Ctrl+Tab
//! walk previews each session it passes through by selecting it, but leaves `Mru`
//! alone until the walk is committed, so `ActiveId != Mru[0]` is a preview and the
//! commit is the edge.
//!
//! **A session is named by its own title alone.** That is the `tree`'s `title`,
//! the focused pane's program title, which for a pane running the agent is the
//! console title this dashboard writes, forwarded through WSL and tmux where they
//! sit between, so it is reported as [`Naming::OwnTitle`]. The file's own `Name`
//! is not read: it is a custom name or agwinterm's `session N`, and a custom name
//! says nothing about what runs in the session. The tree is asked when a
//! departure or input is reported, so a session that answers with no title, or
//! is gone from the tree, is reported unnamed and names no row. The panes'
//! directories are deliberately not passed on either: `attention::resolve_row`
//! would join a session to a row by its directory, making any shell sitting in a
//! project's directory an attention sensor for that project's row. So a title
//! that parses as one of this dashboard's is the only handle allowed to name a
//! row, as it is for Windows Terminal.
//!
//! **A write places a switch only loosely.** The file says what was selected when
//! it was written, never when that changed: the change is up to one settle older
//! when the writer was idle, older still when a held or dropped rename delayed
//! it, and no older than the newest moment the old selection was known to be
//! live — the previous write of the same window, or a later moment agwinterm's
//! pipe confirmed it (see [`Watch::confirm_live`]). So a departure carries two
//! instants. The earlier is the earliest the switch can have been: a row is
//! marked read at it, since erring early only leaves a row showing, and the
//! person verdict's activation rule reads it, since a switch placed early is
//! taken for the click that brought the window forward more often, not less. The
//! later is where the write places the switch, which the verdict's input rule
//! reads, since there an estimate that is late refuses more input as too old.
//!
//! The confirmation is what keeps the activation rule from refusing every first
//! switch after the user comes back to agwinterm. Without it the earliest bound
//! is usually the previous write, often from before the window came forward, so
//! the rule could not tell a switch made after reading from the click that
//! brought the window forward. A pipe read [`super::ACTIVATION_MS`] after each
//! activation settles it: the old selection still live then means the switch
//! came after; another selection live means the file is behind the screen, and
//! its tracking is forgotten so the late write departs nothing.
//!
//! Everything here is pure so it runs on any platform's test build;
//! `agwinterm` reads the files, asks the pipe, and owns the watch.

use std::collections::HashMap;

use serde_json::Value;

use super::agwinterm_wire::{self as wire, Shell};
use super::window_files::{FileRead, Stepped, WindowFiles};
// The seam's word for whether a selection is the live one, renamed here because
// this module's own `Selection` is what a window file says is selected.
use super::{Cover, Front, InputFacts, LastInput, Naming, Observation, ObservationKind, Occupant, Selection as Live, Since, Switch, TerminalSession};

/// The slug every agwinterm log line and observation carries.
pub const TERMINAL: &str = "agwinterm";

/// The data directory agwinterm uses when nothing says otherwise: a release
/// build's. A Debug build uses `agwinterm-dev`.
const DEFAULT_APP_ID: &str = "agwinterm";

/// How long agwinterm's state writer lets a burst of saves settle before it
/// writes, from `new StateWriter(TimeSpan.FromMilliseconds(200), …)`.
///
/// The settle starts when the writer thread wakes and is not pushed back by
/// later saves, so a selection change made while the writer was idle is on disk
/// this long after it happened, and one made inside a settle another save had
/// started is on disk sooner. Stamping a write's change this long before the
/// write therefore errs early. It is not a bound the other way: a save that
/// arrives while the writer is still renaming an earlier file waits for that
/// rename, and agwinterm's own `StateWriter` records an endpoint-security filter
/// holding a rename for 20 to 50 seconds. [`Writer`] is what notices that.
pub const SETTLE_MS: i64 = 200;

/// How far behind its own write time a file may arrive before the writer is
/// taken to have been held up: the watch's coalesce (120 ms) and a read, with a
/// little room. Anything later was a rename that waited, and every save queued
/// behind it waited as long, so a threshold above this would let a rename held
/// for most of a second stamp the switches behind it that much late.
pub const STALL_MS: i64 = 300;

/// The name of agwinterm's data directory under `%LOCALAPPDATA%`, from
/// `AGWINTERM_APP_ID` when it is set.
///
/// agwinterm exports that variable to every process it starts, so a dashboard
/// started from inside an agwinterm session watches the instance hosting it,
/// and one started at login watches the release build, matching how the control
/// pipe is chosen.
pub fn app_id(env: Option<&str>) -> String {
    env.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(DEFAULT_APP_ID).to_string()
}

/// agwinterm's window class, shared by every window it creates.
const WINDOW_CLASS: &str = "AgwintermWin32";

/// Whether a top-level window is one of the library windows of the agwinterm
/// instance whose files this adapter reads.
///
/// The class alone does not say so. Every agwinterm window has it, including the
/// quick terminal, which is a window of its own and never becomes the frontmost
/// library window, and every window of another build, whose files are elsewhere.
/// A library window's caption is its instance's app id (`AppName => _appId`),
/// and agwinterm never changes it; the quick terminal's is
/// `agwinterm quick terminal`.
pub fn is_library_window(class: &str, caption: &str, app_id: &str) -> bool {
    class == WINDOW_CLASS && caption == app_id
}

/// Which library window agwinterm says it activated last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frontmost {
    Named(String),
    /// agwinterm could not be asked, or named no window. The reason goes to the
    /// log.
    Unknown(&'static str),
}

impl Frontmost {
    /// From a `window.list` result, or from `None` when the pipe did not answer.
    pub fn from_list(result: Option<&Value>) -> Self {
        match result.map(super::agwinterm_wire::active_window) {
            Some(Some(w)) => Frontmost::Named(w),
            Some(None) => Frontmost::Unknown("no_active_window"),
            None => Frontmost::Unknown("pipe_unanswered"),
        }
    }

    pub fn window(&self) -> Option<&str> {
        match self {
            Frontmost::Named(w) => Some(w),
            Frontmost::Unknown(_) => None,
        }
    }
}

/// What a window file says is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// A selection the user settled on: `ActiveId` is the front of `Mru`.
    Committed(String),
    /// A Ctrl+Tab walk is previewing some session. `from` is where the walk
    /// started, the front of `Mru`, which is still the committed selection.
    Preview { from: String },
}

/// One session as the window file records it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The shell profile it was started from, `None` for agwinterm's default.
    pub profile: Option<String>,
}

/// One window file, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowFile {
    pub selection: Selection,
    /// Every session, by agwinterm's session id.
    pub sessions: HashMap<String, Entry>,
}

/// Read one window file, or say why it cannot be read.
///
/// The reasons go to the decision log. A missing or empty `ActiveId` and a
/// missing `Mru` are both refused, because without them a preview cannot be
/// told from a selection and every reading would be a guess.
pub fn parse_window(value: &Value) -> Result<WindowFile, &'static str> {
    let active = value.get("ActiveId").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("no_active_id")?;
    let recent = value.get("Mru").and_then(Value::as_array).and_then(|m| m.first()).and_then(Value::as_str).ok_or("no_mru")?;
    let selection = if active == recent { Selection::Committed(active.to_string()) } else { Selection::Preview { from: recent.to_string() } };
    let text = |s: &Value, key: &str| s.get(key).and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string);
    let workspaces = value.get("Workspaces").and_then(Value::as_array).into_iter().flatten();
    let nodes = workspaces.flat_map(|w| w.get("Sessions").and_then(Value::as_array).into_iter().flatten());
    let sessions = nodes.filter_map(|s| Some((s.get("Id").and_then(Value::as_str)?.to_string(), Entry { profile: text(s, "Profile") }))).collect();
    Ok(WindowFile { selection, sessions })
}

/// Session `id` as the seam names it, from a `tree` of its window: by its own
/// title, with no directory, and unnamed where the tree did not answer, does not
/// list it, or gives it no title. See the module doc for why the directory is
/// left out.
pub fn session(tree: Option<&Value>, id: &str) -> TerminalSession {
    TerminalSession { cwd: None, title: tree.and_then(|t| wire::view_of(t, id)).and_then(|v| v.title) }
}

/// What the watch remembers about one window between reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    /// agwinterm's id for the committed selection. The key a departure is
    /// detected on, and deliberately not a row id: two sessions can share a row,
    /// and switching between them is a real departure from the first.
    pub committed: String,
    /// The committed session as last recorded, kept because a session closed
    /// while selected is gone from the file that reports leaving it, and its
    /// profile still says whether its root is a shell.
    pub entry: Entry,
    /// When a Ctrl+Tab walk away from the committed session was first seen, or
    /// `None` when no walk is in progress.
    pub left_at: Option<i64>,
    /// The write time of the file that made `committed` the selection, which is
    /// no earlier than the switch to it, so input up to it may be that switch.
    pub since_ms: i64,
    /// The newest instant `committed` was known to be the selection: the write
    /// time of the newest file read into this tracking, which still showed it,
    /// or a later moment agwinterm's pipe confirmed it live. The next switch
    /// away is no older than this.
    pub seen_ms: i64,
}

/// A session the user left, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Departure {
    /// agwinterm's id for the session left, for asking the terminal about it.
    pub id: String,
    pub entry: Entry,
    /// The earliest the user can have left, which the row is marked read at and
    /// the verdict's activation rule reads.
    pub at_ms: i64,
    /// Where the write that carried the switch places it: one settle before the
    /// write, or the held writer's floor. Late when a save was dropped, so only
    /// the person verdict's input rule uses it, where late refuses more.
    pub switched_ms: i64,
}

/// How long after a switch's placement the facts about it may be read and still
/// describe it: the settle, then the watch's coalesce and a read, which
/// [`STALL_MS`] already allows for. A held writer's floor places a switch
/// further back than this, so its facts are unknown.
pub const FACTS_ALLOWANCE_MS: i64 = SETTLE_MS + STALL_MS;

impl Departure {
    /// The departure as the seam reports it, with the facts read about the
    /// switch at `read_ms`, which are unknown when read too long after it (see
    /// [`super::facts_at_switch`]), and the session named, and what runs in it
    /// judged, from `tree`, a `tree` of its window asked after the switch.
    pub fn observation(&self, tree: Option<&Value>, root_is_shell: bool, front: Front, front_since: Since, last_input: LastInput, read_ms: i64) -> Observation {
        let (front, last_input) = super::facts_at_switch(front, last_input, read_ms, self.switched_ms, FACTS_ALLOWANCE_MS);
        let switch = Switch::Placed { latest_ms: self.switched_ms, front, front_since, last_input };
        Observation { terminal: TERMINAL, session: session(tree, &self.id), at_ms: self.at_ms, kind: ObservationKind::Departed(switch), occupant: occupant_in(tree, &self.id, root_is_shell), naming: Naming::OwnTitle }
    }
}

/// Advance one window's tracking by one read of its file.
///
/// `written_ms` is the file's write time and `floor` what [`Writer`] knows about
/// the writer having been held up. See the module doc for the two instants a
/// departure carries; the stamp is the earliest of one settle before the write,
/// the floor, and the previous write of this window. Returns the new tracking,
/// or `None` to forget the window, and the departure the read reveals, if any.
///
/// A change of committed selection is a departure, decided by
/// [`super::departure_stamp`] like every other adapter's. Writes that leave the
/// selection alone, which includes every context write and clear this dashboard sends,
/// depart nothing.
///
/// **The previous write bounds the stamp** because a switch can reach the disk
/// long after it happened: a save whose rename fails is dropped, and the switch
/// it carried first appears in whatever save comes next, which can be any other
/// change to the window: a program retitling a pane, a session opened or closed.
/// Stamped at that later write, the departure would mark as read the content
/// that caused it. The previous write still showed the old selection, so the
/// switch cannot be older than it.
///
/// **A walk is credited to its start.** The user stopped looking at the
/// committed session when the walk's first preview appeared, not when it was
/// committed, so that first preview's instant is kept and becomes the departure's
/// stamp. A walk cancelled back to where it began departs nothing.
///
/// **Whatever cannot be placed in time is forgotten rather than stamped.** A
/// window first seen mid-walk, or one whose committed selection moved while this
/// watch was not looking (a commit coalesced with the start of the next walk),
/// has a departure somewhere in the past at an unknown instant. Stamping it now
/// would be later than the truth and would mark as read content that arrived
/// after the user had gone; forgetting only leaves a row showing.
pub fn step(prev: Option<&Tracked>, file: &WindowFile, written_ms: i64, floor: Option<i64>) -> (Option<Tracked>, Option<Departure>) {
    let switched_ms = (written_ms - SETTLE_MS).min(floor.unwrap_or(i64::MAX));
    let at_ms = switched_ms.min(prev.map_or(i64::MAX, |p| p.seen_ms));
    let entry = |id: &str, fallback: Option<&Entry>| file.sessions.get(id).or(fallback).cloned().unwrap_or_default();
    match (&file.selection, prev) {
        (Selection::Committed(now), prev) => {
            let since_ms = match prev {
                Some(p) if p.committed == *now && p.left_at.is_none() => p.since_ms,
                _ => written_ms,
            };
            let tracked = Tracked { committed: now.clone(), entry: entry(now, None), left_at: None, since_ms, seen_ms: written_ms };
            let departure = prev.and_then(|p| super::departure_stamp(Some(&p.committed), now, |a: &str, b: &str| a == b, p.left_at, at_ms).map(|at_ms| Departure { id: p.committed.clone(), entry: entry(&p.committed, Some(&p.entry)), at_ms, switched_ms }));
            (Some(tracked), departure)
        }
        (Selection::Preview { from }, Some(p)) if *from == p.committed => (Some(Tracked { committed: p.committed.clone(), entry: entry(from, Some(&p.entry)), left_at: p.left_at.or(Some(at_ms)), since_ms: p.since_ms, seen_ms: written_ms }), None),
        (Selection::Preview { .. }, _) => (None, None),
    }
}

/// What the watch has learned about agwinterm's writer being held up.
///
/// A held rename is visible after the fact: the file it carried was written
/// (its mtime set) before the rename stuck, so it reaches the watch far behind
/// its own write time. Every save queued behind it is written only after it,
/// whichever file it belongs to, so a change in one of those may be as old as
/// the held write's start.
///
/// It sees only renames in the watched directory. A held write of the window
/// index, which sits outside it, delays the window files just the same and is
/// not seen, and neither is a rename still held when a switch is read. A rename
/// that failed outright leaves nothing to see at all; [`step`]'s bound by the
/// previous write covers that one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Writer {
    /// From the held write's time to when it reached the watch.
    held: Option<(i64, i64)>,
}

impl Writer {
    /// Record one file's arrival: written at `written_ms`, read at
    /// `observed_ms`.
    pub fn note(&mut self, written_ms: i64, observed_ms: i64) {
        if observed_ms - written_ms > STALL_MS {
            self.held = Some((written_ms, observed_ms));
        }
    }

    /// The earliest a change carried by a write at `written_ms` can have
    /// happened, where that is earlier than the settle alone says: a write made
    /// after a held one and soon after it was released. The settle is allowed
    /// twice over for the release, since the writer's wait can overshoot.
    pub fn floor(&self, written_ms: i64) -> Option<i64> {
        self.held.filter(|&(start, end)| written_ms > start && written_ms - SETTLE_MS <= end + SETTLE_MS).map(|(start, _)| start - SETTLE_MS)
    }
}

/// One changed window file, as a pass saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub window: String,
    /// `switched` when there is a departure, else why there is none:
    /// `preview`, `first_sight`, `same_session` or `unplaced`.
    pub outcome: &'static str,
    pub departure: Option<Departure>,
}

/// The watch's whole state between passes.
pub struct Watch {
    pub files: WindowFiles<Tracked>,
    writer: Writer,
}

impl Default for Watch {
    fn default() -> Self {
        Self { files: WindowFiles::new(TERMINAL), writer: Writer::default() }
    }
}

impl Watch {
    /// One pass over the window files, read at `observed_ms`: every file whose
    /// write time moved is stepped, oldest first, and reported. A baseline is a
    /// first sighting of every window and teaches nothing about the writer,
    /// since every file in it was written before the watch started.
    ///
    /// Tracking advances whatever is then decided about a person, so a scripted
    /// switch the verdict refuses still moves the window on to the session it
    /// switched to.
    pub fn pass(&mut self, files: Vec<FileRead>, baseline: bool, observed_ms: i64) -> Vec<Changed> {
        let writer = &mut self.writer;
        self.files.pass(files, baseline, |window, prev, text, written_ms| {
            let file = match serde_json::from_str::<Value>(text).map_err(|_| "unparseable").and_then(|v| parse_window(&v)) {
                Ok(file) => file,
                Err(reason) => return Stepped::Unknown(reason),
            };
            if !baseline {
                writer.note(written_ms, observed_ms);
            }
            let (next, departure) = step(prev, &file, written_ms, writer.floor(written_ms));
            let outcome = match (&departure, &file.selection, prev, &next) {
                (Some(_), ..) => "switched",
                (None, Selection::Preview { .. }, _, Some(_)) => "preview",
                (None, _, Some(_), None) => "unplaced",
                (None, _, None, _) => "first_sight",
                (None, ..) => "same_session",
            };
            Stepped::Read(next, Changed { window: window.to_string(), outcome, departure })
        })
    }

    /// Forget `window`'s tracking when it still holds `committed`, a selection
    /// agwinterm has just said is no longer the live one.
    ///
    /// The file is behind the screen then, a save held or dropped, so the write
    /// that catches up carries a switch it cannot place in time; forgotten, that
    /// write is a first sighting and departs nothing. Tracking that has moved on
    /// since `committed` was read is left alone, since the write it waited for
    /// has arrived.
    pub fn forget_lagging(&mut self, window: &str, committed: &str) {
        if self.files.tracked().get(window).is_some_and(|t| t.committed == committed) {
            self.files.forget(window);
        }
    }

    /// Take what agwinterm's pipe said at `at_ms` about whether `committed` is
    /// `window`'s live selection.
    ///
    /// Live, it was still selected at `at_ms`, so the next switch away is no
    /// older than that and the tracking's `seen_ms` moves up to it. Not live,
    /// the file is behind the screen and the tracking is forgotten, as
    /// [`forget_lagging`](Self::forget_lagging) does. `at_ms` must be no later
    /// than the pipe's reading, so the instant the request was sent. Tracking
    /// that has moved on since `committed` was read is left alone, and so is a
    /// window mid-walk, whose live selection is a preview and says nothing about
    /// the committed one.
    pub fn confirm_live(&mut self, window: &str, committed: &str, live: bool, at_ms: i64) {
        let Some(t) = self.files.tracked_mut(window).filter(|t| t.committed == committed && t.left_at.is_none()) else { return };
        if live {
            t.seen_ms = t.seen_ms.max(at_ms);
        } else {
            self.files.forget(window);
        }
    }
}

/// The session on screen in `window`, or `None` when that window is not tracked
/// or is mid-walk.
///
/// Input arriving during a walk goes to the walk, not to the committed session
/// it is walking away from, so it is credited to nobody.
pub fn on_screen<'a>(tracked: &'a HashMap<String, Tracked>, window: &str) -> Option<&'a Tracked> {
    tracked.get(window).filter(|t| t.left_at.is_none())
}

/// Whether the window a switch happened in is in front: a library window of
/// this instance holds the foreground, and agwinterm says the window it activated
/// last, which is that one, is this one. Two facts, because a window file names a
/// window id and the foreground names a window handle.
pub fn front_of(window: &str, library_window_in_front: bool, frontmost: &Frontmost) -> Front {
    match frontmost {
        _ if !library_window_in_front => Front::No,
        Frontmost::Unknown(reason) => Front::Unknown(reason),
        Frontmost::Named(w) if w != window => Front::No,
        Frontmost::Named(_) => Front::Yes,
    }
}

/// What runs in a session, from what the tree says runs in its panes and whether
/// its root process is a shell.
///
/// An idle shell is told apart by the tree. A session whose root is not a shell
/// agwinterm recognizes, such as one started from a WSL profile, reads as busy
/// whatever runs in it, so it is not taken for the agent. A shell busy with some
/// other child process reads as the agent, which the tree cannot tell apart.
pub fn occupant(shell: Shell, root_is_shell: bool) -> Occupant {
    match shell {
        Shell::Bare => Occupant::Shell,
        Shell::Unknown => Occupant::Unknown("shell_unknown"),
        Shell::Occupied if !root_is_shell => Occupant::Unknown("unrecognized_root"),
        Shell::Occupied => Occupant::Agent,
    }
}

/// What runs in session `id`, from a `tree` of its window, or unknown when the
/// tree did not answer or does not list it.
pub fn occupant_in(tree: Option<&Value>, id: &str, root_is_shell: bool) -> Occupant {
    occupant(tree.and_then(|t| wire::view_of(t, id)).map_or(Shell::Unknown, |v| v.shell), root_is_shell)
}

/// The shells agwinterm recognizes by executable name (`ForegroundShellNames`).
const SHELLS: [&str; 8] = ["cmd", "powershell", "pwsh", "bash", "sh", "zsh", "fish", "nu"];

/// Whether a session started from `profile` runs a shell agwinterm recognizes as
/// its root process, by agwinterm's own launch rule over its `profiles.json`.
///
/// agwinterm resolves a profile by name without regard to case, else the
/// configured default, else the first profile, and launches its `command`. A
/// file that could not be read, or lists no profile, is answered `false`: the
/// profiles agwinterm then runs with are detected at its start and are not on
/// disk. A session started with an explicit command rather than a profile is
/// recorded under no profile and so is judged by the default, which is a limit
/// of the file.
pub fn root_is_shell(profile: Option<&str>, profiles: Option<&Value>) -> bool {
    let Some(config) = profiles else { return false };
    let listed = config.get("profiles").and_then(Value::as_array).into_iter().flatten();
    let list: Vec<(String, &str)> = listed.filter_map(|p| Some((p.get("name")?.as_str()?.trim().to_lowercase(), p.get("command")?.as_str()?))).filter(|(n, c)| !n.is_empty() && !c.trim().is_empty()).collect();
    let named = |name: &str| list.iter().find(|(n, _)| *n == name.trim().to_lowercase()).map(|(_, c)| *c);
    let Some(&(_, first)) = list.first() else { return false };
    let default = config.get("default").and_then(Value::as_str).unwrap_or("Windows PowerShell");
    let command = profile.filter(|p| !p.trim().is_empty()).and_then(named).or_else(|| named(default)).unwrap_or(first);
    let exe = command.trim().rsplit(['\\', '/']).next().unwrap_or_default().to_lowercase();
    SHELLS.contains(&exe.strip_suffix(".exe").unwrap_or(&exe))
}

/// Input at `at_ms` to the session `on_screen` says is selected in the window
/// agwinterm says is in front, with what `tree`, asked now, says of it, its name
/// included.
///
/// - **The selection** is live only where agwinterm lists the session as its
///   selection now. The tracking is the window file, which a held or dropped save
///   leaves behind the screen; a switch made after the input would have been
///   input itself, so the two agreeing now means they agreed when the input was
///   made.
/// - **It began** with the write that committed it, which is no earlier than the
///   switch to it.
/// - **A cover** is an overlay, a terminal drawn over the session's output that
///   takes its input. The scratch cover is not in the tree, so typing into a
///   session's scratch pane is credited to the session.
pub fn input_observation(on_screen: &Tracked, tree: Option<&Value>, root_is_shell: bool, front: Front, front_since: Since, at_ms: i64) -> Observation {
    let view = tree.map(|t| wire::view_of(t, &on_screen.committed));
    let (selection, cover) = match view {
        None => (Live::Unknown("tree_unanswered"), Cover::Unknown),
        Some(None) => (Live::Lagging, Cover::Unknown),
        Some(Some(v)) => (if v.active { Live::Live } else { Live::Lagging }, if v.covered { Cover::Covered } else { Cover::Clear }),
    };
    let facts = InputFacts { front, front_since, selection, selected_since: Since::At(on_screen.since_ms), cover };
    let occupant = occupant_in(tree, &on_screen.committed, root_is_shell);
    Observation { terminal: TERMINAL, session: session(tree, &on_screen.committed), at_ms, kind: ObservationKind::Input(facts), occupant, naming: Naming::OwnTitle }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::window_files::Contents;

    /// A window file read from agwinterm 0.20.13.1 on 2026-10-02, with the
    /// emoji as the surrogate-pair escapes agwinterm writes and the three
    /// pane-level fields this module does not read trimmed. The pane `Command` is
    /// the captured foreground command agwinterm restores, a WSL launch on this
    /// machine, and session-level `Cwd` is empty, as it is live: the directory
    /// lives on the pane.
    const WINDOW: &str = r#"{
        "Workspaces":[{"Id":"dfa60a39-3fe2-4c90-ade4-a7bd1883b780","Name":"workspace 1","Expanded":true,"Sessions":[
            {"Id":"409d0a9a-e59d-47d4-a7e1-6270f9918635","Name":"\ud83d\udfe2 agwinterm","CustomName":"\ud83d\udfe2 agwinterm","Profile":null,"Active":0,"Flagged":false,
             "Panes":[{"Id":"409d0a9a-e59d-47d4-a7e1-6270f9918635","Cwd":"C:\\src\\external\\agwinterm","FontSize":16,"Ratio":1,"Command":"\"C:\\WINDOWS\\system32\\wsl.exe\" -d Ubuntu -- /mnt/c/src/tools/start.sh"}],
             "Cwd":"","FontSize":0,"BgFile":null,"BgOpacity":15,"BgMode":"fit"},
            {"Id":"69892e75-7d33-4f18-b3af-df79e40ce7d9","Name":"\ud83d\udfe2 claude","CustomName":"\ud83d\udfe2 claude","Profile":null,"Active":0,"Flagged":false,
             "Panes":[{"Id":"69892e75-7d33-4f18-b3af-df79e40ce7d9","Cwd":"C:\\src\\claude","FontSize":16,"Ratio":1,"Command":null}],
             "Cwd":"","FontSize":0,"BgFile":null,"BgOpacity":15,"BgMode":"fit"},
            {"Id":"c18abb52-0589-46ed-9010-4d2a552c7a86","Name":"\u23f3 ai-dashboard","CustomName":"\u23f3 ai-dashboard","Profile":null,"Active":0,"Flagged":false,
             "Panes":[{"Id":"c18abb52-0589-46ed-9010-4d2a552c7a86","Cwd":"C:\\src\\tauri-dashboard","FontSize":16,"Ratio":1,"Command":null}],
             "Cwd":"","FontSize":0,"BgFile":null,"BgOpacity":15,"BgMode":"fit"}
        ]}],
        "ActiveId":"c18abb52-0589-46ed-9010-4d2a552c7a86",
        "SidebarWidth":220,"SidebarVisible":true,"WindowX":674,"WindowY":0,"WindowWidth":1040,"WindowHeight":660,"WindowMaximized":true,
        "SidebarMode":"tree","FocusedWorkspaceId":null,
        "Mru":["c18abb52-0589-46ed-9010-4d2a552c7a86","69892e75-7d33-4f18-b3af-df79e40ce7d9","409d0a9a-e59d-47d4-a7e1-6270f9918635"]
    }"#;

    /// `profiles.json` as agwinterm seeds it on this machine, trimmed to the
    /// fields read.
    const PROFILES: &str = r#"{"default":"Windows PowerShell","profiles":[
        {"name":"Windows PowerShell","command":"powershell.exe"},
        {"name":"PowerShell 7","command":"pwsh.exe"},
        {"name":"Git Bash","command":"C:\\Program Files\\Git\\bin\\bash.exe","args":["-i","-l"]},
        {"name":"WSL: Ubuntu","command":"C:\\WINDOWS\\system32\\wsl.exe","args":["-d","Ubuntu"]}
    ]}"#;

    const AGW: &str = "409d0a9a-e59d-47d4-a7e1-6270f9918635";
    const CLAUDE: &str = "69892e75-7d33-4f18-b3af-df79e40ce7d9";
    const DASH: &str = "c18abb52-0589-46ed-9010-4d2a552c7a86";

    fn live() -> Value {
        serde_json::from_str(WINDOW).unwrap()
    }

    /// The live file with its selection moved: `active` selected, `mru` as the
    /// recency order.
    fn moved(active: &str, mru: &[&str]) -> Value {
        let mut v = live();
        v["ActiveId"] = Value::from(active);
        v["Mru"] = Value::from(mru.to_vec());
        v
    }

    fn selecting(active: &str, mru: &[&str]) -> WindowFile {
        parse_window(&moved(active, mru)).unwrap()
    }

    fn entry_of(id: &str) -> Entry {
        selecting(id, &[id]).sessions[id].clone()
    }

    /// Tracking of `committed` whose previous write bounds nothing, so a test
    /// sees the settle rule alone; the bound by the previous write has its own
    /// tests.
    fn tracking(committed: &str, left_at: Option<i64>) -> Tracked {
        Tracked { committed: committed.to_string(), entry: entry_of(committed), left_at, since_ms: 0, seen_ms: i64::MAX }
    }

    fn departure(id: &str, at_ms: i64) -> Departure {
        Departure { id: id.to_string(), entry: entry_of(id), at_ms, switched_ms: at_ms }
    }

    /// `step` with the settle backed off, as the watch calls it for an idle
    /// writer: a write at `at + SETTLE_MS` stamps `at`.
    fn stepped(prev: Option<&Tracked>, file: &WindowFile, at: i64) -> (Option<Tracked>, Option<Departure>) {
        step(prev, file, at + SETTLE_MS, None)
    }

    #[test]
    fn the_live_file_reads_as_a_committed_selection_with_every_session_listed() {
        let w = parse_window(&live()).unwrap();
        assert_eq!(w.selection, Selection::Committed(DASH.into()));
        assert_eq!(w.sessions.len(), 3);
        assert_eq!(w.sessions[AGW].profile, None, "a session on the default profile");
    }

    #[test]
    fn a_selection_ahead_of_the_recency_order_is_a_walk_preview() {
        // Mid-walk agwinterm selects each session it passes and leaves `Mru`
        // alone, so the front of `Mru` is still where the walk started.
        assert_eq!(selecting(CLAUDE, &[DASH, CLAUDE, AGW]).selection, Selection::Preview { from: DASH.into() });
    }

    #[test]
    fn a_file_without_its_selection_or_recency_is_refused_not_guessed_at() {
        for (key, reason) in [("ActiveId", "no_active_id"), ("Mru", "no_mru")] {
            let mut v = live();
            v.as_object_mut().unwrap().remove(key);
            assert_eq!(parse_window(&v), Err(reason), "{key} missing");
        }
        let mut v = live();
        v["ActiveId"] = Value::Null;
        assert_eq!(parse_window(&v), Err("no_active_id"), "a null selection");
        v["ActiveId"] = Value::from("");
        assert_eq!(parse_window(&v), Err("no_active_id"), "an empty one");
        let mut v = live();
        v["Mru"] = Value::from(Vec::<String>::new());
        assert_eq!(parse_window(&v), Err("no_mru"), "an empty recency order cannot tell a preview from a selection");
    }

    /// A `tree` of one window listing session `id` titled `title`, with a child
    /// process in its one pane.
    fn titled_tree(id: &str, title: Option<&str>) -> Value {
        serde_json::json!({ "workspaces": [{ "sessions": [{ "id": id, "name": "session 1", "title": title, "foregroundShells": [null] }] }] })
    }

    /// `titled_tree` with the title this dashboard writes for DASH's row.
    fn agent_tree(id: &str) -> Value {
        titled_tree(id, Some("⏳ ai-dashboard"))
    }

    /// A departure as a person makes one: the window in front for a minute and
    /// input at the switch, from a session running the agent under `tree`, read
    /// promptly.
    fn person_in(d: &Departure, tree: &Value) -> Observation {
        d.observation(Some(tree), true, Front::Yes, Since::At(d.switched_ms - 60_000), LastInput::At(d.switched_ms), d.switched_ms + SETTLE_MS)
    }

    /// A finished, unread row, as `attention::resolve_row` judges it.
    fn unread_row(id: &str) -> crate::state::AgentSession {
        let state = crate::state::AppState::new();
        state.apply_set(
            crate::state::SetInput { delegated_task: None, message_line: None, message_is_reply: None, id: id.into(), status: crate::state::Status::Done, label: None, source: None, model: None, input_tokens: None, dialog_entry: None, waiting_backstop_armed: false, turn_from_relay: None },
            0,
            &[],
            None,
        );
        state.snapshot().pop().expect("row")
    }

    #[test]
    fn a_departed_session_is_named_by_its_own_title_and_by_nothing_else() {
        let rows = [unread_row("claude")];
        let (_, d) = stepped(Some(&tracking(CLAUDE, None)), &selecting(DASH, &[DASH, CLAUDE]), 5_000);
        let d = d.expect("a departure");
        let resolve = |tree: &Value| {
            let o = person_in(&d, tree);
            assert_eq!(o.naming, Naming::OwnTitle);
            crate::attention::resolve_row(&o.session, &rows, None)
        };
        // The agent's console title, forwarded to the pane, names its row, and
        // the window file's custom name beside it (`🟢 claude`, an earlier
        // dashboard's) plays no part.
        assert_eq!(resolve(&titled_tree(CLAUDE, Some("🟢 claude [68%]"))), crate::attention::Resolved::Row("claude".into(), crate::terminals::NamedBy::Title));
        // A plain shell in the `claude` row's directory: no title, or a title
        // this dashboard did not write. Its directory derives the row, and the
        // session must still name none.
        for title in [None, Some("claude"), Some("~/projects/claude")] {
            let o = person_in(&d, &titled_tree(CLAUDE, title));
            assert_eq!(o.session, TerminalSession { cwd: None, title: title.map(str::to_string) }, "{title:?}");
            assert_eq!(resolve(&titled_tree(CLAUDE, title)), crate::attention::Resolved::Unknown, "{title:?}");
        }
        // A tree that did not answer, or no longer lists the session: unnamed,
        // never filled in from the file.
        assert_eq!(d.observation(None, true, Front::Yes, Since::At(0), LastInput::At(d.switched_ms), d.switched_ms).session.title, None);
        assert_eq!(person_in(&d, &titled_tree(DASH, Some("🟢 claude"))).session.title, None, "another session's title");
    }

    #[test]
    fn an_agent_in_a_wsl_session_is_credited_by_its_own_title() {
        // A WSL profile's root is no shell agwinterm recognizes, so what runs in
        // the session is unknown; the title the agent's console carries is still
        // its own, and a person's switch away from it counts.
        let (_, d) = stepped(Some(&tracking(CLAUDE, None)), &selecting(DASH, &[DASH, CLAUDE]), 5_000);
        let d = d.unwrap();
        let o = d.observation(Some(&titled_tree(CLAUDE, Some("🟢 claude"))), false, Front::Yes, Since::At(d.switched_ms - 60_000), LastInput::At(d.switched_ms), d.switched_ms + SETTLE_MS);
        assert_eq!(o.occupant, Occupant::Unknown("unrecognized_root"));
        assert_eq!(crate::terminals::person_verdict(&o, crate::terminals::NamedBy::Title), Ok(()));
        // An idle shell left behind under the agent's last title is refused.
        let mut bare = titled_tree(CLAUDE, Some("🟢 claude"));
        bare["workspaces"][0]["sessions"][0]["foregroundShells"] = serde_json::json!(["pwsh"]);
        assert_eq!(crate::terminals::person_verdict(&person_in(&d, &bare), crate::terminals::NamedBy::Title), Err(crate::terminals::Refusal::BareShell));
    }

    #[test]
    fn the_first_reading_of_a_window_departs_nothing() {
        // At startup every window's selection is new, and nothing was left.
        let (tracked, departure) = stepped(None, &selecting(DASH, &[DASH]), 1_000);
        assert_eq!(tracked.map(|t| (t.committed, t.since_ms, t.seen_ms)), Some((DASH.to_string(), 1_000 + SETTLE_MS, 1_000 + SETTLE_MS)));
        assert_eq!(departure, None);
    }

    #[test]
    fn a_rewrite_that_leaves_the_selection_alone_departs_nothing() {
        // A rename, or a program retitling a session, rewrites the file and
        // keeps `ActiveId`.
        let mut v = live();
        v["Workspaces"][0]["Sessions"][2]["Name"] = Value::from("🟢 ai-dashboard");
        let (tracked, departure) = stepped(Some(&tracking(DASH, None)), &parse_window(&v).unwrap(), 5_000);
        assert_eq!(departure, None);
        let tracked = tracked.unwrap();
        assert_eq!((tracked.since_ms, tracked.seen_ms), (0, 5_000 + SETTLE_MS), "the selection began where it did, and the newest write showed it");
    }

    #[test]
    fn a_departure_is_stamped_one_settle_before_the_write_that_carries_it() {
        // The writer's settle starts at the save, so the switch is that much
        // older than the file's write time.
        let (_, d) = step(Some(&tracking(DASH, None)), &selecting(CLAUDE, &[CLAUDE, DASH, AGW]), 5_000, None);
        assert_eq!(d, Some(departure(DASH, 5_000 - SETTLE_MS)));
        // A floor from a held writer is taken when it is earlier.
        let (_, d) = step(Some(&tracking(DASH, None)), &selecting(CLAUDE, &[CLAUDE, DASH, AGW]), 25_000, Some(1_000));
        assert_eq!(d.map(|d| (d.at_ms, d.switched_ms)), Some((1_000, 1_000)));
    }

    #[test]
    fn a_switch_whose_save_was_dropped_is_stamped_no_later_than_the_write_before_it() {
        // The user left DASH at 3 000, and the save carrying that was dropped
        // when its rename failed. DASH's agent finished at 9 000, and the next
        // save, a session opened in the window, is written at 9 450: it shows
        // CLAUDE selected. The last write the watch read, at
        // 1 000, still showed DASH, so the switch is no older than that, and the
        // row is not marked read at 9 250, after the content that arrived.
        let mut read = tracking(DASH, None);
        read.seen_ms = 1_000;
        let (_, d) = step(Some(&read), &selecting(CLAUDE, &[CLAUDE, DASH, AGW]), 9_450, None);
        let d = d.unwrap();
        assert_eq!(d.at_ms, 1_000);
        assert_eq!(d.switched_ms, 9_450 - SETTLE_MS, "the verdict is still asked about the switch where the write places it");
    }

    #[test]
    fn leaving_one_session_for_another_departs_the_one_left_behind() {
        let (tracked, departure) = stepped(Some(&tracking(DASH, None)), &selecting(CLAUDE, &[CLAUDE, DASH, AGW]), 5_000);
        assert_eq!(departure.map(|d| (d.id, d.at_ms)), Some((DASH.to_string(), 5_000)));
        let tracked = tracked.unwrap();
        assert_eq!((tracked.committed.as_str(), tracked.since_ms), (CLAUDE, 5_000 + SETTLE_MS), "the new selection began with the write that committed it");
    }

    #[test]
    fn a_session_closed_while_selected_keeps_the_profile_that_was_remembered() {
        // Closing the active session selects the next one, and the file that
        // reports the departure no longer holds the session departed from.
        let mut v = live();
        v["Workspaces"][0]["Sessions"].as_array_mut().unwrap().remove(2);
        v["ActiveId"] = Value::from(CLAUDE);
        v["Mru"] = Value::from(vec![CLAUDE, AGW]);
        let remembered = Tracked { entry: Entry { profile: Some("WSL: Ubuntu".into()) }, ..tracking(DASH, None) };
        let (_, departure) = stepped(Some(&remembered), &parse_window(&v).unwrap(), 5_000);
        assert_eq!(departure.unwrap().entry.profile.as_deref(), Some("WSL: Ubuntu"));
    }

    #[test]
    fn a_walk_preview_is_not_a_departure_and_its_start_is_remembered() {
        let (tracked, departure) = stepped(Some(&tracking(DASH, None)), &selecting(CLAUDE, &[DASH, CLAUDE, AGW]), 2_000);
        assert_eq!(departure, None);
        assert_eq!(tracked.as_ref().map(|t| (t.committed.as_str(), t.left_at)), Some((DASH, Some(2_000))));
        let (tracked, departure) = stepped(tracked.as_ref(), &selecting(AGW, &[DASH, CLAUDE, AGW]), 3_000);
        assert_eq!(departure, None);
        assert_eq!(tracked.unwrap().left_at, Some(2_000), "a later preview keeps the walk's first instant");
    }

    #[test]
    fn a_committed_walk_departs_from_where_it_started_at_the_walks_start() {
        // The user stopped looking at the session when the walk left it, so the
        // commit is credited to the first preview rather than to itself.
        let walking = tracking(DASH, Some(2_000));
        let (tracked, d) = stepped(Some(&walking), &selecting(AGW, &[AGW, DASH, CLAUDE]), 4_000);
        assert_eq!(d.map(|d| (d.id, d.at_ms, d.switched_ms)), Some((DASH.to_string(), 2_000, 4_000)), "the verdict asks about the commit, which is where the last key was");
        assert_eq!(tracked.map(|t| (t.committed, t.left_at)), Some((AGW.to_string(), None)));
    }

    #[test]
    fn a_walk_cancelled_back_to_its_start_departs_nothing_and_restarts_the_selection() {
        // Coming back from a walk is an arrival like any other: what changed in
        // the committed session while the walk was elsewhere was not on screen.
        let (tracked, departure) = stepped(Some(&tracking(DASH, Some(2_000))), &selecting(DASH, &[DASH, CLAUDE, AGW]), 4_000);
        assert_eq!(departure, None);
        assert_eq!(tracked.map(|t| (t.left_at, t.since_ms)), Some((None, 4_000 + SETTLE_MS)), "and the walk is over");
    }

    #[test]
    fn a_window_first_seen_mid_walk_is_not_tracked_until_the_walk_ends() {
        // Where the walk began is known, when it began is not, so a commit away
        // from it could not be stamped early enough. Not tracking makes that
        // commit a first sighting.
        assert_eq!(stepped(None, &selecting(CLAUDE, &[DASH, CLAUDE]), 2_000), (None, None));
        let (tracked, departure) = stepped(None, &selecting(CLAUDE, &[CLAUDE, DASH]), 3_000);
        assert_eq!((tracked.map(|t| t.committed), departure), (Some(CLAUDE.to_string()), None));
    }

    #[test]
    fn a_walk_from_a_selection_this_watch_never_saw_committed_is_forgotten() {
        // DASH was committed, a click to CLAUDE and the start of a walk landed in
        // one write, so the file shows a walk from CLAUDE. The departure from DASH
        // happened at an unknown instant and is dropped rather than stamped late.
        assert_eq!(stepped(Some(&tracking(DASH, None)), &selecting(AGW, &[CLAUDE, DASH, AGW]), 4_000), (None, None));
    }

    #[test]
    fn a_writer_held_up_by_a_rename_floors_the_writes_queued_behind_it() {
        // A save written at 1 000 had its rename held for 20 seconds. The
        // user switched sessions at 2 000; the switch's write could only be made
        // once the rename returned, so its write time says 21 000.
        let mut w = Writer::default();
        w.note(1_000, 21_100);
        assert_eq!(w.floor(21_000 + SETTLE_MS), Some(1_000 - SETTLE_MS), "the switch is credited no later than the held write began");
        assert_eq!(w.floor(1_000), None, "the held write itself carries what was current when it was made");
        assert_eq!(w.floor(60_000), None, "a write long after the release was made by an idle writer");
        let mut idle = Writer::default();
        idle.note(5_000, 5_000 + SETTLE_MS);
        assert_eq!(idle.floor(5_100), None, "a write that arrives promptly says nothing about the writer");
    }

    #[test]
    fn a_rename_held_for_less_than_a_second_still_floors_the_writes_behind_it() {
        // A rename written at 1 000 was held 700 ms, and a switch at 1 100 was
        // written behind it at 1 900. Only the coalesce and a read separate an
        // idle write from its arrival, so 700 ms late is a held writer.
        let mut w = Writer::default();
        w.note(1_000, 1_820);
        assert_eq!(w.floor(1_900), Some(1_000 - SETTLE_MS));
    }

    fn file(window: &str, value: &Value, written: i64) -> FileRead {
        FileRead { window: window.into(), contents: Contents::Read(value.to_string(), written) }
    }

    #[test]
    fn a_held_writer_seen_in_one_pass_floors_the_switch_read_in_the_same_pass() {
        // The held rename and the switch it delayed arrive together; the held one
        // is older and is stepped first, so the switch is already floored.
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH, CLAUDE]), 500), file("w2", &moved(AGW, &[AGW]), 500)], true, 600);
        let changed = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 21_200), file("w2", &moved(AGW, &[AGW]), 1_000)], false, 21_300);
        let switched: Vec<_> = changed.iter().filter_map(|c| c.departure.clone()).collect();
        // The stamp is the baseline's write, which still showed DASH and is
        // earlier still; the verdict is asked about the floor.
        assert_eq!(switched, vec![Departure { switched_ms: 1_000 - SETTLE_MS, ..departure(DASH, 500) }]);
    }

    #[test]
    fn a_switch_behind_a_held_writer_is_refused_because_its_facts_are_read_too_late() {
        // The switch's write was held for 20 seconds and read at 21 300, while
        // the floor places the switch at 800. The window in front and the input
        // clock read then describe the last 20 seconds, not the switch: a script
        // switching while the user was away, followed by the user scrolling,
        // would read like a person's switch.
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH, CLAUDE]), 500), file("w2", &moved(AGW, &[AGW]), 500)], true, 600);
        let changed = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 21_200), file("w2", &moved(AGW, &[AGW]), 1_000)], false, 21_300);
        let d = changed.iter().find_map(|c| c.departure.clone()).unwrap();
        let front = front_of("w1", true, &Frontmost::Named("w1".into()));
        let judged = |last_input, read_ms| crate::terminals::person_verdict(&d.observation(Some(&agent_tree(DASH)), true, front, Since::At(-100_000), LastInput::At(last_input), read_ms), crate::terminals::NamedBy::Title);
        assert_eq!(judged(21_000, 21_300), Err(crate::terminals::Refusal::FrontUnknown("read_after_switch")));
        // Facts read within a settle and a stall of the placement are the
        // switch's own, and are judged on their merits.
        assert_eq!(judged(700, d.switched_ms + FACTS_ALLOWANCE_MS), Ok(()));
    }

    /// The window file as tracked after a save at 1 000 that showed DASH
    /// selected.
    fn dash_saved_at_1000() -> Watch {
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH, CLAUDE]), 1_000)], true, 1_100);
        watch
    }

    #[test]
    fn the_click_that_brought_the_window_forward_and_switched_is_refused_when_its_save_was_dropped() {
        // The window came forward at 50 000 by a click on CLAUDE in its sidebar;
        // the save carrying the switch was dropped, and the next save, 20 seconds
        // later, carries it. Its write places the switch at 69 800, long after
        // the activation, though DASH was behind a browser until the click.
        let mut watch = dash_saved_at_1000();
        let late = |watch: &mut Watch| watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 70_000)], false, 70_100);
        // Without a check after the activation, the earliest bound is the save
        // at 1 000, so the activation rule refuses it all the same.
        let unchecked = late(&mut dash_saved_at_1000()).pop().and_then(|c| c.departure).unwrap();
        let judged = |d: &Departure| crate::terminals::person_verdict(&d.observation(Some(&agent_tree(DASH)), true, Front::Yes, Since::At(50_000), LastInput::At(69_900), 70_100), crate::terminals::NamedBy::Title);
        assert_eq!(judged(&unchecked), Err(crate::terminals::Refusal::ActivatedBySwitch));
        // The check after the activation finds CLAUDE live where the file says
        // DASH, so the file is behind the screen and its late write departs
        // nothing.
        watch.confirm_live("w1", DASH, false, 50_501);
        let caught_up = late(&mut watch);
        assert_eq!((caught_up[0].outcome, caught_up[0].departure.clone()), ("first_sight", None));
    }

    #[test]
    fn a_selection_confirmed_live_after_the_activation_lets_the_first_switch_after_it_count() {
        // The user came back at 50 000 without switching, read DASH, and clicked
        // CLAUDE at 60 000. The check after the activation found DASH still
        // live, so the switch away from it came later than that.
        let mut watch = dash_saved_at_1000();
        watch.confirm_live("w1", DASH, true, 50_501);
        let d = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 60_200)], false, 60_300).pop().and_then(|c| c.departure).unwrap();
        assert_eq!((d.at_ms, d.switched_ms), (50_501, 60_000));
        let o = d.observation(Some(&agent_tree(DASH)), true, Front::Yes, Since::At(50_000), LastInput::At(59_990), 60_300);
        assert_eq!(crate::terminals::person_verdict(&o, crate::terminals::NamedBy::Title), Ok(()));
        // Without it, the earliest bound is the save at 1 000, and the rule
        // cannot tell this switch from the click that brought the window forward.
        let unconfirmed = dash_saved_at_1000().pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 60_200)], false, 60_300).pop().and_then(|c| c.departure).unwrap();
        let o = unconfirmed.observation(Some(&agent_tree(DASH)), true, Front::Yes, Since::At(50_000), LastInput::At(59_990), 60_300);
        assert_eq!(crate::terminals::person_verdict(&o, crate::terminals::NamedBy::Title), Err(crate::terminals::Refusal::ActivatedBySwitch));
    }

    #[test]
    fn a_confirmation_never_moves_a_tracking_it_was_not_about() {
        let mut watch = dash_saved_at_1000();
        watch.confirm_live("w1", CLAUDE, true, 50_501);
        assert_eq!(watch.files.tracked()["w1"].seen_ms, 1_000, "another selection than the one tracked");
        watch.confirm_live("w1", DASH, true, 500);
        assert_eq!(watch.files.tracked()["w1"].seen_ms, 1_000, "an older confirmation than the newest write");
        watch.confirm_live("w1", CLAUDE, false, 50_501);
        assert!(watch.files.tracked().contains_key("w1"), "a lag reported about a selection no longer tracked");
        // Mid-walk, the live selection is a preview, which says nothing about the
        // committed one.
        watch.pass(vec![file("w1", &moved(CLAUDE, &[DASH, CLAUDE]), 2_000)], false, 2_100);
        watch.confirm_live("w1", DASH, false, 50_501);
        assert_eq!(watch.files.tracked()["w1"].left_at, Some(1_000));
    }

    #[test]
    fn a_departure_the_verdict_refuses_still_moves_the_window_on() {
        // The pass reports every switch and the caller drops a scripted one; the
        // next switch must depart from where the scripted one left the window,
        // not from where it was before.
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH]), 1_000)], true, 1_100);
        let scripted = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 2_000)], false, 2_100);
        assert_eq!(scripted[0].departure.as_ref().map(|d| (d.id.as_str(), d.at_ms)), Some((DASH, 1_000)), "bounded by the write before it");
        let human = watch.pass(vec![file("w1", &moved(AGW, &[AGW, CLAUDE, DASH]), 3_000)], false, 3_100);
        assert_eq!(human[0].departure.as_ref().map(|d| (d.id.as_str(), d.at_ms)), Some((CLAUDE, 2_000)));
    }

    #[test]
    fn a_window_back_from_an_unknown_shape_departs_nothing() {
        // Good, unparseable, then good with a different selection: the switch
        // happened somewhere in the gap and cannot be stamped.
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH]), 1_000)], true, 1_100);
        let broken = FileRead { window: "w1".into(), contents: Contents::Read("{\"Workspaces\":[]}".into(), 2_000) };
        assert!(watch.pass(vec![broken], false, 2_100).is_empty());
        let back = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 3_000)], false, 3_100);
        assert_eq!((back[0].outcome, back[0].departure.clone()), ("first_sight", None));
    }

    #[test]
    fn each_changed_file_says_why_it_departed_nothing() {
        let mut watch = Watch::default();
        let outcomes = |c: Vec<Changed>| c.into_iter().map(|c| c.outcome).collect::<Vec<_>>();
        assert_eq!(outcomes(watch.pass(vec![file("w1", &moved(DASH, &[DASH, CLAUDE]), 1_000)], true, 1_100)), ["first_sight"]);
        let mut renamed = moved(DASH, &[DASH, CLAUDE]);
        renamed["Workspaces"][0]["Sessions"][2]["Name"] = Value::from("🟢 ai-dashboard");
        assert_eq!(outcomes(watch.pass(vec![file("w1", &renamed, 2_000)], false, 2_100)), ["same_session"]);
        assert_eq!(outcomes(watch.pass(vec![file("w1", &moved(CLAUDE, &[DASH, CLAUDE]), 3_000)], false, 3_100)), ["preview"]);
        assert_eq!(outcomes(watch.pass(vec![file("w1", &moved(AGW, &[CLAUDE, DASH]), 4_000)], false, 4_100)), ["unplaced"]);
    }

    #[test]
    fn a_file_found_behind_the_live_selection_is_forgotten_so_its_late_write_departs_nothing() {
        // The poll asked agwinterm and found CLAUDE live while the file still
        // said DASH: the switch's save is held or lost. When it lands, it is a
        // first sighting rather than a departure stamped at the late write.
        let mut watch = Watch::default();
        watch.pass(vec![file("w1", &moved(DASH, &[DASH]), 1_000)], true, 1_100);
        watch.forget_lagging("w1", DASH);
        let late = watch.pass(vec![file("w1", &moved(CLAUDE, &[CLAUDE, DASH]), 30_000)], false, 30_100);
        assert_eq!((late[0].outcome, late[0].departure.clone()), ("first_sight", None));
        // Tracking that moved on since the poll read it is not the lagging one.
        watch.forget_lagging("w1", DASH);
        assert!(watch.files.tracked().contains_key("w1"));
    }

    #[test]
    fn the_session_on_screen_is_the_windows_committed_one() {
        let tracked = HashMap::from([("w1".to_string(), tracking(DASH, None)), ("w2".to_string(), tracking(CLAUDE, Some(1_000)))]);
        assert_eq!(on_screen(&tracked, "w1").map(|t| t.committed.as_str()), Some(DASH));
        assert_eq!(on_screen(&tracked, "w2"), None, "input during a walk goes to the walk");
        assert_eq!(on_screen(&tracked, "w3"), None, "a window not tracked");
    }

    #[test]
    fn the_frontmost_window_comes_from_agwinterm_or_is_unknown() {
        let list = serde_json::json!({ "windows": [{ "id": "w1", "open": true, "active": true }] });
        assert_eq!(Frontmost::from_list(Some(&list)), Frontmost::Named("w1".into()));
        assert_eq!(Frontmost::from_list(Some(&serde_json::json!({ "windows": [] }))), Frontmost::Unknown("no_active_window"));
        assert_eq!(Frontmost::from_list(None), Frontmost::Unknown("pipe_unanswered"));
        assert_eq!(Frontmost::from_list(None).window(), None);
    }

    const W1: &str = "499dc388-2497-4072-b1d9-05a339f9a03b";

    #[test]
    fn the_window_that_switched_is_in_front_only_when_agwinterm_and_the_desktop_agree() {
        let named = |w: &str| Frontmost::Named(w.into());
        assert_eq!(front_of(W1, true, &named(W1)), Front::Yes);
        // A pipe `session select` while the user types in Windows Terminal, in
        // agwinterm's quick terminal, or in another agwinterm build.
        assert_eq!(front_of(W1, false, &named(W1)), Front::No);
        assert_eq!(front_of(W1, true, &named("other-window")), Front::No, "a background agwinterm window");
        assert_eq!(front_of(W1, true, &Frontmost::Unknown("pipe_unanswered")), Front::Unknown("pipe_unanswered"), "a pipe that did not answer is not a script");
    }

    #[test]
    fn only_a_library_window_of_this_instance_is_in_front() {
        assert!(is_library_window("AgwintermWin32", "agwinterm", "agwinterm"));
        assert!(!is_library_window("AgwintermWin32", "agwinterm quick terminal", "agwinterm"), "the quick terminal is its own window");
        assert!(!is_library_window("AgwintermWin32", "agwinterm-dev", "agwinterm"), "another build keeps other files");
        assert!(is_library_window("AgwintermWin32", "agwinterm-dev", "agwinterm-dev"));
        assert!(!is_library_window("CASCADIA_HOSTING_WINDOW_CLASS", "agwinterm", "agwinterm"), "a Windows Terminal tab titled agwinterm");
    }

    /// DASH on screen since a commit written at 1 000.
    fn dash_on_screen() -> Tracked {
        Tracked { since_ms: 1_000, ..tracking(DASH, None) }
    }

    /// A `tree` of one window listing DASH with these fields.
    fn tree(active: bool, shells: Value, overlay: bool) -> Value {
        serde_json::json!({ "workspaces": [{ "sessions": [{ "id": DASH, "name": "session 3", "title": "⏳ ai-dashboard", "active": active, "foregroundShells": shells, "overlay": overlay }] }] })
    }

    fn input_facts(o: &Observation) -> InputFacts {
        match o.kind {
            ObservationKind::Input(facts) => facts,
            ObservationKind::Departed(_) => panic!("an input observation"),
        }
    }

    #[test]
    fn input_carries_what_agwinterm_says_of_the_selection_now() {
        let o = input_observation(&dash_on_screen(), Some(&tree(true, serde_json::json!([null]), false)), true, Front::Yes, Since::At(0), 5_000);
        assert_eq!(input_facts(&o), InputFacts { front: Front::Yes, front_since: Since::At(0), selection: Live::Live, selected_since: Since::At(1_000), cover: Cover::Clear });
        assert_eq!((o.occupant, o.naming, o.at_ms), (Occupant::Agent, Naming::OwnTitle, 5_000));
        assert_eq!(o.session.title.as_deref(), Some("⏳ ai-dashboard"), "the title, not the name beside it");
        assert_eq!(crate::terminals::person_verdict(&o, crate::terminals::NamedBy::Title), Ok(()));
    }

    #[test]
    fn input_is_not_credited_while_the_file_is_behind_the_live_selection() {
        // The user clicked CLAUDE and its save is held, so the file still says
        // DASH while agwinterm says DASH is not selected. Typing in CLAUDE must
        // not mark DASH read.
        let not_active = input_observation(&dash_on_screen(), Some(&tree(false, serde_json::json!([null]), false)), true, Front::Yes, Since::At(0), 5_000);
        assert_eq!(input_facts(&not_active).selection, Live::Lagging);
        let unlisted = input_observation(&dash_on_screen(), Some(&serde_json::json!({ "workspaces": [] })), true, Front::Yes, Since::At(0), 5_000);
        assert_eq!((input_facts(&unlisted).selection, input_facts(&unlisted).cover), (Live::Lagging, Cover::Unknown), "a session the tree no longer lists");
        let silent = input_observation(&dash_on_screen(), None, true, Front::Yes, Since::At(0), 5_000);
        assert_eq!((input_facts(&silent).selection, input_facts(&silent).cover, silent.occupant), (Live::Unknown("tree_unanswered"), Cover::Unknown, Occupant::Unknown("shell_unknown")));
    }

    #[test]
    fn the_click_that_arrived_at_a_session_is_not_reading_it() {
        // The click that selected DASH is the desktop's last input, and it
        // landed just before the write that committed DASH.
        let judged = |at_ms| crate::terminals::person_verdict(&input_observation(&dash_on_screen(), Some(&tree(true, serde_json::json!([null]), false)), true, Front::Yes, Since::At(0), at_ms), crate::terminals::NamedBy::Title);
        assert_eq!(judged(1_000 - SETTLE_MS), Err(crate::terminals::Refusal::SwitchInput));
        assert_eq!(judged(1_000 + crate::terminals::SWITCH_RELEASE_MS), Err(crate::terminals::Refusal::SwitchInput), "nor the release of that click");
        assert_eq!(judged(1_000 + crate::terminals::SWITCH_RELEASE_MS + 1), Ok(()));
    }

    #[test]
    fn an_overlay_over_the_session_is_a_cover() {
        let o = input_observation(&dash_on_screen(), Some(&tree(true, serde_json::json!([null]), true)), true, Front::Yes, Since::At(0), 5_000);
        assert_eq!(input_facts(&o).cover, Cover::Covered);
    }

    #[test]
    fn a_session_is_the_agent_only_when_the_tree_and_its_root_say_so() {
        assert_eq!(occupant(Shell::Occupied, true), Occupant::Agent);
        assert_eq!(occupant(Shell::Bare, true), Occupant::Shell);
        assert_eq!(occupant(Shell::Unknown, true), Occupant::Unknown("shell_unknown"));
        // A WSL session reads as busy whether or not an agent runs in it, so the
        // tree cannot tell it from a shell running the agent.
        assert_eq!(occupant(Shell::Occupied, false), Occupant::Unknown("unrecognized_root"));
        assert_eq!(occupant_in(Some(&tree(true, serde_json::json!(["pwsh"]), false)), DASH, true), Occupant::Shell);
        assert_eq!(occupant_in(Some(&tree(true, serde_json::json!(["pwsh"]), false)), CLAUDE, true), Occupant::Unknown("shell_unknown"), "a session the tree does not list");
        assert_eq!(occupant_in(None, DASH, true), Occupant::Unknown("shell_unknown"), "a tree that did not answer");
    }

    #[test]
    fn a_sessions_root_is_its_profiles_command_by_agwintermss_own_rule() {
        let profiles: Value = serde_json::from_str(PROFILES).unwrap();
        let p = Some(&profiles);
        assert!(root_is_shell(None, p), "no profile is the default, Windows PowerShell");
        assert!(root_is_shell(Some("git bash"), p), "a name matched without regard to case, its executable a recognized shell");
        assert!(!root_is_shell(Some("WSL: Ubuntu"), p), "wsl.exe is no shell agwinterm recognizes");
        assert!(root_is_shell(Some("a profile since deleted"), p), "an unknown name falls back to the default");
        let mut wsl_default = profiles.clone();
        wsl_default["default"] = Value::from("WSL: Ubuntu");
        assert!(!root_is_shell(None, Some(&wsl_default)));
        let mut no_default = profiles.clone();
        no_default["default"] = Value::from("gone");
        assert!(root_is_shell(None, Some(&no_default)), "a default that names nothing is the first profile");
        assert!(!root_is_shell(None, None), "a file that could not be read says nothing");
        assert!(!root_is_shell(None, Some(&serde_json::json!({ "profiles": [] }))), "nor does a list agwinterm would replace with detected shells");
    }

    #[test]
    fn the_data_directory_follows_the_app_id_agwinterm_exports() {
        assert_eq!(app_id(None), "agwinterm");
        assert_eq!(app_id(Some("  ")), "agwinterm");
        assert_eq!(app_id(Some("agwinterm-dev")), "agwinterm-dev");
    }
}
