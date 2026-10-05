//! The agterm adapter: reading which session the user is in, and when they left.
//!
//! **The departure is the primary signal, and only the watch reports one.** The
//! selected session going from S to T means the user *left* S, and that is what
//! marks S read — leaving is the moment you are done with what was on screen.
//! Arriving at T marks nothing: you have not read T yet, and marking on arrival
//! meant merely passing through a finished tab marked it. Sampling a *level*
//! (which tab is selected) to detect an *edge* misses any visit that begins and
//! ends between two samples, and a switch found that way happened at an instant
//! nobody can name, which the verdict refuses. [`AgtermAdapter::watch`] takes the
//! edge from agterm's per-window snapshot files instead, which agterm writes on a
//! ~0.3s debounce, so a short visit writes *twice* there. What survives is the
//! debounce itself — an enter-and-leave inside 300ms still coalesces to one
//! write. See `.claude/memory/attention_visit_detection_design.md`. While the
//! watch cannot run — no snapshot directory, or a snapshot layout this adapter
//! does not know — no departure is credited.
//!
//! **The poll brings input, and asks agterm nothing unless it could count.** Input
//! is the desktop's last input, credited to the session selected in agterm's
//! active window only while agterm is the frontmost application and has held the
//! foreground since before the input, so the poll asks `NSWorkspace`, in process,
//! before it spends a subprocess: with another application in front, nothing is
//! asked of agterm at all. agterm's own per-window `idleMs` is not used. What it
//! counts has not been measured, and a window behind another application still
//! receives scroll and hover events, so it could not say the window was in front.
//!
//! Both are per *window*: each window has its own selection, and a bare `tree`
//! projects only the frontmost one, so every read names its window.
//!
//! **Whether a person made it is not decided here.** Each observation carries the
//! facts [`super::agterm_facts`] reads — whether agterm and the window were in
//! front and since when, the input clock, what runs in the session and whether
//! an overlay covers it — and `crate::attention` applies
//! [`super::person_verdict`], the rule every terminal shares. Since when a window
//! has been in front is agterm's own activation, which with several windows open
//! can be earlier than the window's; see [`super::agterm_facts`]. The activation
//! rule there cannot tell a covered
//! window from a visible one, so the click straight onto the next session of an
//! agterm window being read beside another application is refused; see
//! [`super::ACTIVATION_MS`]. agterm's titles are the sessions' own OSC titles,
//! which this dashboard writes only onto the agent's tty, but an observation also
//! carries the session's working directory, and a row named through that alone
//! could have been named by a plain shell in the project's directory; the verdict
//! asks what runs in the session there, and refuses a known shell everywhere.

use std::collections::HashMap;

use serde_json::Value;

use super::agterm_facts as facts;
use super::agterm_wire as wire;
use super::window_files::Stepped;
use super::{Front, InputFacts, LabelTarget, LabelWrite, LastInput, Naming, Observation, ObservationKind, Selection, Since, Switch, TerminalSession};
#[cfg(target_os = "macos")]
use super::window_files::WindowFiles;
#[cfg(target_os = "macos")]
use super::{SelectionClock, TerminalAdapter};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

const NAME: &str = "agterm";

/// When each window's selection began, keyed by window id and compared by
/// agterm's session id, written by the watch from each file's write time and by
/// the poll from its own clock.
#[cfg(target_os = "macos")]
type Selections = Arc<Mutex<SelectionClock<String>>>;

/// Each window's tracking, written by the watch from the snapshot files and by
/// the activation check from what agterm says is selected.
#[cfg(target_os = "macos")]
type Files = Arc<Mutex<WindowFiles<Seen>>>;

#[cfg(target_os = "macos")]
pub struct AgtermAdapter {
    selections: Selections,
    files: Files,
}

#[cfg(target_os = "macos")]
impl Default for AgtermAdapter {
    fn default() -> Self {
        Self { selections: Arc::default(), files: Arc::new(Mutex::new(WindowFiles::new(NAME))) }
    }
}

#[cfg(target_os = "macos")]
impl TerminalAdapter for AgtermAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Watch agterm's per-window snapshots and report a departure the moment the
    /// selection changes — ~300ms after the switch, agterm's own save debounce,
    /// rather than at the next tick.
    ///
    /// The directory watch itself, and why it watches the directory and reads
    /// once before the first change, is `snapshot_watch`'s, and the per-window
    /// bookkeeping around the diff is `window_files`', both shared with the
    /// agwinterm adapter on Windows. What is agterm's is the diff, [`step`]: only
    /// a changed `selectedSessionID` counts, since the same save is triggered by
    /// renames, moves, reordering, sidebar width and recency. The record of which
    /// application is in front starts here too, with the check each of agterm's
    /// activations gets ([`confirm_activations`]).
    fn watch(&self, sink: std::sync::mpsc::Sender<Observation>) {
        let Some(dir) = windows_dir() else {
            tracing::warn!(terminal = NAME, "no HOME; the selection watcher cannot start");
            return;
        };
        let (activations, rx) = std::sync::mpsc::channel();
        facts::observe_activation(activations);
        let files = self.files.clone();
        std::thread::spawn(move || confirm_activations(&files, &rx));
        let selections = self.selections.clone();
        let files = self.files.clone();
        super::snapshot_watch::spawn(NAME, dir, move |dir, baseline| reread(dir, baseline, &files, &selections), sink);
    }

    /// Every tab agterm currently holds, across every workspace of every open
    /// window.
    ///
    /// Per-window, because a bare `tree` projects only the **frontmost** window,
    /// and asking again returns that same window, so a session in a background
    /// one would not merely be late — it would be permanently invisible, and the
    /// caller's "is anything missing?" gate would stay true forever, paying a
    /// subprocess on every tick to rediscover the same nothing.
    ///
    /// A window it cannot read is skipped rather than failing the whole answer: a
    /// window closing between the list and the read is ordinary, and the sessions
    /// in the windows that *did* answer are still worth returning. `None` is
    /// reserved for having been unable to ask agterm at all, which is the only
    /// case the caller must not read as "there are no tabs".
    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        walk_windows("restorable", |_, nodes| nodes.iter().map(session_from_node).collect())
    }

    /// Whether this agterm serves a `session context` verb at all.
    ///
    /// A version check rather than a probe of the verb: 0.25.0 answers
    /// `unexpected arguments: 'context'`, and a failed write is logged, so
    /// probing would put a refusal on a log line once per start for a terminal
    /// that is simply older. Answered from one `version` call, which needs no
    /// open window.
    ///
    /// `None` means agterm could not be asked — it is not running yet, which at
    /// login is the ordinary case — and the caller retries. `Some(false)` is a
    /// settled no, and stands the labeller down for the life of the process.
    fn can_label(&self) -> Option<bool> {
        let value = crate::agterm::agtermctl(&["version", "--json"])?;
        let version = wire::app_version(&value)?;
        let serves = wire::supports_context(version);
        if !serves {
            tracing::info!(terminal = NAME, version, "this agterm has no session context verb; not labelling");
        }
        Some(serves)
    }

    /// Every session in every open window, with the title that joins it to a row
    /// and the context it is showing now.
    ///
    /// The same walk [`sessions`](Self::sessions) makes, and for the reason its
    /// doc gives: a bare `tree` projects only the frontmost window. A window that
    /// does not answer is skipped rather than failing the pass, since its
    /// sessions are simply not labellable this time round; `None` is reserved for
    /// not having reached agterm at all, which the caller retries rather than
    /// reading as a terminal with nothing in it.
    fn label_targets(&self) -> Option<Vec<LabelTarget>> {
        walk_windows("labellable", wire::targets_from)
    }

    /// Write or clear one session's context.
    ///
    /// Through [`crate::agterm::agtermctl_checked`] rather than `agtermctl`, so a
    /// refusal arrives as agterm's own sentence and reaches the `label_write` log
    /// line. The text is already inside the byte budget — `labels::plan` fits it
    /// to [`LabelTarget::budget`] — so a `context must be at most 256 UTF-8
    /// bytes` here means the fitter and this adapter disagree about the limit,
    /// which is worth reading in a log rather than silently retrying.
    fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
        let (window, session) = wire::split_key(key).ok_or_else(|| format!("not a key this adapter minted: {key}"))?;
        let argv = match write {
            LabelWrite::Context(text) => wire::context_argv(window, session, text),
            LabelWrite::ClearContext => wire::clear_argv(window, session),
        };
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        crate::agterm::agtermctl_checked(&args).map(|_| ())
    }

    /// Input to the session on screen in agterm's active window.
    ///
    /// Asked in an order that spends least where nothing could count: whether
    /// agterm is in front, in process, then agterm's window list and one `tree`
    /// of its active window, two subprocesses per tick and only while agterm is
    /// in front. The window list is read fresh on every tick, because the active
    /// window decides who the input is credited to. The input clock is read
    /// after agterm has answered, so the instant it gives is no earlier than the
    /// truth by the time the calls took. The reading also confirms the watch's tracking of that window
    /// ([`take_reading`]), so the next switch there is placed after this tick.
    fn poll(&mut self, _now_ms: i64) -> Vec<Observation> {
        let skip = |outcome: &'static str, why: &str| {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome, "{why}");
            Vec::new()
        };
        let app = facts::agterm_frontmost();
        match app {
            facts::AppFront::Agterm => {}
            facts::AppFront::Other => return skip(super::Refusal::NotInFront.slug(), "another application is in front, so no input can be agterm's"),
            facts::AppFront::Unreadable => return skip(super::Refusal::FrontUnknown("frontmost_unreadable").slug(), "the system named no frontmost application"),
        }
        let windows = facts::windows(crate::agterm::agtermctl(&["window", "list", "--json"]).as_ref());
        let facts::Windows::Listed { active: Some(window), .. } = &windows else {
            return skip("no_active_window", "agterm named no active window");
        };
        let asked_at = crate::commands::now_ms();
        let Some(tree) = crate::agterm::agtermctl(&["tree", "--json", "--window", window]) else {
            return skip("no_answer", "agterm gave no tree");
        };
        let Some(reading) = parse_reading(&tree) else {
            return skip("no_selection", "the tree names no selected session");
        };
        let Some(idle) = crate::idle::idle_ms() else {
            return skip("no_input_clock", "the desktop's input clock could not be read");
        };
        let read_ms = crate::commands::now_ms();
        let (selected_since, confirmed) = take_reading(&mut self.files.lock().unwrap(), &mut self.selections.lock().unwrap(), window, &reading, asked_at, read_ms);
        let front_since = facts::front_since();
        let observation = input_observation(&reading, facts::front(app, &windows, window), front_since, selected_since, read_ms - idle as i64);
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome = "input", window = %window, selected = ?reading.selected.title, front_since = ?front_since, confirmed, "agterm poll");
        vec![observation]
    }
}

/// agterm's persisted per-window state, one file per window.
///
/// Private, undocumented state with no compatibility promise, which is why
/// [`SNAPSHOT_VERSION`] is checked and an unexpected shape stands the window down
/// rather than being guessed at.
#[cfg(target_os = "macos")]
fn windows_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Application Support/agterm/windows"))
}

/// agterm's own save debounce, from `AppStore.scheduleSave`. A selection change
/// reaches disk this long after it happened when nothing else is saved in
/// between, so a write's change is placed this long before the write. A save
/// made while further changes keep arriving can come later, which places the
/// switch late, so this is only the latest bound; the earliest is the previous
/// write of the window ([`step`]). It is also the residual blind window, since an
/// enter-and-leave inside it coalesces into one write carrying only the final
/// state.
const SAVE_DEBOUNCE_MS: i64 = 300;

/// How long after a switch's placement the facts about it may be read and still
/// describe it: the save debounce, the watch's coalesce, and a directory read
/// and a `window list` call, with room. See [`super::facts_at_switch`].
const FACTS_ALLOWANCE_MS: i64 = SAVE_DEBOUNCE_MS + 500;

/// The `version` this adapter knows how to read. A different one means the
/// layout may have moved under us; the watcher stands that window down, said
/// once at warn, rather than producing confident wrong departures. No departure
/// from that window is credited until it reads again; the poll's input
/// observations carry on.
const SNAPSHOT_VERSION: u64 = 1;

/// What one window's snapshot says: which session is selected, and every
/// session's working directory.
///
/// The file carries **no title**, which is why a departure still costs one
/// `tree` call — `resolve_row` prefers the title because a session that has
/// `cd`-ed reports a cwd deriving another row's id. The cwd here is the fallback
/// when that call fails, which is a worse answer than the title and a much better
/// one than nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub selected_session_id: Option<String>,
    pub cwds: HashMap<String, String>,
}

/// Read one window snapshot, or `None` if it is not a shape we know.
///
/// Pure and fixture-pinned. Atomic writes mean there are no torn reads, so a
/// parse failure here is a real signal about the schema rather than a race.
pub fn parse_snapshot(value: &Value) -> Option<Snapshot> {
    if value.get("version").and_then(Value::as_u64) != Some(SNAPSHOT_VERSION) {
        return None;
    }
    let mut cwds = HashMap::new();
    for ws in value.get("workspaces").and_then(Value::as_array).into_iter().flatten() {
        for s in ws.get("sessions").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(id), Some(cwd)) = (s.get("id").and_then(Value::as_str), s.get("cwd").and_then(Value::as_str)) {
                cwds.insert(id.to_string(), cwd.to_string());
            }
        }
    }
    Some(Snapshot { selected_session_id: value.get("selectedSessionID").and_then(Value::as_str).map(str::to_string), cwds })
}

/// Re-read every window snapshot and report whoever was departed since last
/// time, with the facts about the switch.
///
/// The bookkeeping is `window_files`' and the diff is [`step`]'s; this notes when
/// each selection began, names the sessions left, and reads the facts. Those
/// that are the same for every departure in the pass — the desktop's input
/// clock, the front application, agterm's windows, the activation record — are
/// read once, the input clock first so the instant it is read at is the pass's,
/// and only when there is a departure.
#[cfg(target_os = "macos")]
fn reread(dir: &std::path::Path, baseline: bool, files: &Files, selections: &Selections) -> Vec<Observation> {
    let Some(read) = super::window_files::read_dir(dir) else { return Vec::new() };
    let departures = files.lock().unwrap().pass(read, baseline, |window, prev, text, written_ms| {
        let stepped = step(window, prev, text, written_ms);
        if let Stepped::Read(Some(seen), _) = &stepped {
            selections.lock().unwrap().note(window.to_string(), &seen.id, |a: &str, b: &str| a == b, written_ms);
        }
        stepped
    });
    let lefts: Vec<Left> = departures.into_iter().flatten().collect();
    if lefts.is_empty() {
        return Vec::new();
    }
    let now_ms = crate::commands::now_ms();
    let last_input = crate::idle::idle_ms().map_or(LastInput::Unknown, |idle| LastInput::At(now_ms - idle as i64));
    let app = facts::agterm_frontmost();
    let windows = facts::windows(crate::agterm::agtermctl(&["window", "list", "--json"]).as_ref());
    let front_since = facts::front_since();
    let mut out = Vec::new();
    for left in lefts {
        // The file has no title, and `resolve_row` wants one — a `cd`-ed session
        // reports a cwd deriving another row's id. One `tree` call of the
        // window on this edge buys it, with what runs in the session; the
        // snapshot's cwd is the fallback when it fails.
        let tree = crate::agterm::agtermctl(&["tree", "--json", "--window", &left.window]);
        let front = facts::front(app, &windows, &left.window);
        let observation = departure(&left, tree.as_ref(), front, front_since, last_input, now_ms);
        tracing::debug!(terminal = NAME, decision = "attention_poll", outcome = "switched", source = "watch", window = %left.window, title = ?observation.session.title, kind = ?observation.kind, occupant = ?observation.occupant, "selection snapshot changed");
        out.push(observation);
    }
    out
}

/// For each of agterm's activations, ask agterm, once [`super::ACTIVATION_MS`]
/// has passed, which session its window has selected, and tell the tracking
/// ([`confirm_live`]).
///
/// The question the verdict's activation rule cannot otherwise answer here:
/// whether a switch reported later was made after agterm came forward, or was
/// the click that brought it forward. A switch is no older than the newest
/// moment its window's previous selection was known, which without this is the
/// window's previous write, often from before the activation, so every first
/// switch after coming back to agterm would be refused. Asked of the window
/// agterm names active, however many are open. The instant recorded is the one
/// the question was sent at, which is no later than agterm's reading of it.
///
/// Only the window active at the activation is asked here. A window brought
/// forward inside agterm raises no activation, so it is the poll's reading that
/// confirms it ([`take_reading`]), one tick after the user arrives in it.
#[cfg(target_os = "macos")]
fn confirm_activations(files: &Files, rx: &std::sync::mpsc::Receiver<i64>) {
    while let Ok(activated_at) = rx.recv() {
        let wait = activated_at + super::ACTIVATION_MS + 1 - crate::commands::now_ms();
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait as u64));
        }
        let skip = |outcome: &'static str, why: &str| tracing::debug!(decision = "attention_poll", terminal = NAME, source = "activation", outcome, activated_at, "{why}");
        let windows = facts::windows(crate::agterm::agtermctl(&["window", "list", "--json"]).as_ref());
        let facts::Windows::Listed { active: Some(window), .. } = &windows else {
            skip("no_active_window", "agterm named no active window");
            continue;
        };
        let asked_at = crate::commands::now_ms();
        let Some(tree) = crate::agterm::agtermctl(&["tree", "--json", "--window", window]) else {
            skip("tree_unanswered", "agterm did not say what is selected in the window that came forward");
            continue;
        };
        let selected = parse_reading(&tree).map(|r| r.session_id);
        let outcome = confirm_live(&mut files.lock().unwrap(), window, selected.as_deref(), asked_at);
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "activation", outcome, window = %window, activated_at, asked_at, "agterm came forward");
    }
}

/// What the watch holds for one window: the session its file shows selected,
/// and the newest instant that session was known to be selected — the write time
/// of the newest file read into this tracking, or a later moment agterm
/// confirmed it. The next switch away is no older than `seen_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    id: String,
    seen_ms: i64,
}

/// Take what agterm said at `at_ms` about which session `window` has selected,
/// `live`, and answer the outcome for the log.
///
/// The tracked session, still selected then, moves its `seen_ms` up to `at_ms`.
/// Another session selected means the file is behind the screen, a switch not yet
/// saved, so the tracking is forgotten and the write that catches up departs
/// nothing: that switch cannot be placed after the activation. No answer about
/// the selection leaves the tracking as it was.
fn confirm_live(files: &mut super::window_files::WindowFiles<Seen>, window: &str, live: Option<&str>, at_ms: i64) -> &'static str {
    let Some(tracked) = files.tracked().get(window).map(|s| s.id.clone()) else { return "untracked" };
    match live {
        Some(id) if id == tracked => {
            if let Some(seen) = files.tracked_mut(window) {
                seen.seen_ms = seen.seen_ms.max(at_ms);
            }
            "selection_confirmed"
        }
        Some(_) => {
            files.forget(window);
            "file_behind_screen"
        }
        None => "no_selection",
    }
}

/// Take the poll's reading of `window`, from a `tree` asked at `asked_at` and in
/// hand at `read_ms`: note since when its selection has been held, and confirm
/// the watch's tracking of the window with it ([`confirm_live`]). Answers that
/// instant and the confirmation's outcome for the log.
///
/// The confirmation is what places a switch in a window brought forward inside
/// agterm after its arrival. No activation reaches [`confirm_activations`] for
/// it, so without this the window's earliest bound stays its last write, older
/// than agterm's activation, and its first switch is refused as the activating
/// click. A selection that differs from the file forgets the tracking, as after
/// an activation, so a raise-and-switch read here before its save departs
/// nothing.
fn take_reading(files: &mut super::window_files::WindowFiles<Seen>, selections: &mut super::SelectionClock<String>, window: &str, reading: &Reading, asked_at: i64, read_ms: i64) -> (i64, &'static str) {
    let selected_since = selections.note(window.to_string(), &reading.session_id, |a: &str, b: &str| a == b, read_ms);
    (selected_since, confirm_live(files, window, Some(&reading.session_id), asked_at))
}

/// A session left behind, by agterm's id, in the window it was left in, with the
/// directory the snapshot gives it and the two instants a departure carries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Left {
    window: String,
    id: String,
    cwd: Option<String>,
    /// The earliest the switch can have been, which the row is marked read at
    /// and the verdict's activation rule reads.
    at_ms: i64,
    /// Where the write places the switch, one debounce before it, which the
    /// verdict's input rule reads.
    switched_ms: i64,
}

/// One window's snapshot, written at `written_ms`, against the window's tracking
/// before it.
///
/// The switch happened *before* the write — agterm coalesces for
/// [`SAVE_DEBOUNCE_MS`] first — and after the previous selection was last known,
/// so the departure carries both: no earlier than the previous write or
/// confirmation, and no later than one debounce before this write. A snapshot
/// with no selection keeps the tracking as it was, and a layout this adapter
/// does not know stands the window down.
fn step(window: &str, prev: Option<&Seen>, text: &str, written_ms: i64) -> Stepped<Seen, Option<Left>> {
    let Some(snapshot) = serde_json::from_str::<Value>(text).ok().as_ref().and_then(parse_snapshot) else {
        return Stepped::Unknown("unrecognized_snapshot");
    };
    let Some(now) = snapshot.selected_session_id else { return Stepped::Read(prev.cloned(), None) };
    let switched_ms = written_ms - SAVE_DEBOUNCE_MS;
    let earliest = switched_ms.min(prev.map_or(i64::MAX, |p| p.seen_ms));
    // agterm hands out a stable session id, so identity is plain equality here.
    let at = prev.and_then(|p| super::departure_stamp(Some(&p.id), &now, |a: &str, b: &str| a == b, Some(earliest), written_ms));
    let left = at.zip(prev).map(|(at_ms, p)| Left { window: window.to_string(), id: p.id.clone(), cwd: snapshot.cwds.get(&p.id).cloned(), at_ms, switched_ms });
    Stepped::Read(Some(Seen { id: now, seen_ms: written_ms }), left)
}

/// The departure of `left`, judged by what a `tree` of its window says now, with
/// the foreground and the desktop's input clock read at `read_ms`.
///
/// The session is named from the tree, its title preferred, and from the
/// snapshot's directory alone when the tree does not list it, in which case what
/// ran in it is unknown. The facts are unknown when read longer than
/// [`FACTS_ALLOWANCE_MS`] after the write's placement of the switch.
fn departure(left: &Left, tree: Option<&Value>, front: Front, front_since: Since, last_input: LastInput, read_ms: i64) -> Observation {
    let node = tree.and_then(|t| crate::agterm::session_node(t, &left.id));
    let session = node.map_or_else(|| TerminalSession { cwd: left.cwd.clone(), title: None }, session_from_node);
    let (front, last_input) = super::facts_at_switch(front, last_input, read_ms, left.switched_ms, FACTS_ALLOWANCE_MS);
    let switch = Switch::Placed { latest_ms: left.switched_ms, front, front_since, last_input };
    Observation { terminal: NAME, session, at_ms: left.at_ms, kind: ObservationKind::Departed(switch), occupant: facts::occupant(node), naming: Naming::OwnTitle }
}

/// Input at `at_ms` to the session `reading` names, which agterm's tree has just
/// listed as its window's selection.
fn input_observation(reading: &Reading, front: Front, front_since: Since, selected_since: i64, at_ms: i64) -> Observation {
    let facts = InputFacts { front, front_since, selection: Selection::Live, selected_since: Since::At(selected_since), cover: reading.cover };
    Observation { terminal: NAME, session: reading.selected.clone(), at_ms, kind: ObservationKind::Input(facts), occupant: reading.occupant, naming: Naming::OwnTitle }
}

/// Ask every open window for its tree and collect what `map` makes of each
/// window's session nodes.
///
/// The walk is shared because its three rules are a contract two callers must
/// not state differently, and both of them answer an `Option` whose `None` the
/// caller acts on:
///
/// * **Per-window, never a bare `tree`**, which projects only the frontmost
///   window — and asking again returns that same window, so a session in a
///   background one would not merely be late but permanently invisible, with the
///   caller's "is anything missing?" gate staying true forever and paying a
///   subprocess per tick to rediscover the same nothing.
/// * **A window that does not answer is skipped, not fatal.** A window closing
///   between the list and the read is ordinary, and the sessions in the windows
///   that did answer are still worth returning.
/// * **`None` means agterm could not be asked at all**, which is the one case a
///   caller must not read as "there are no sessions" — `sessions` would tell
///   `session_restore` every tab had gone, and `label_targets` would have
///   `labels::plan` withdraw every context.
///
/// `what` names the subject in the skipped-window log line, the only part that
/// legitimately differs between callers.
#[cfg(target_os = "macos")]
fn walk_windows<T>(what: &str, map: impl Fn(&str, &[Value]) -> Vec<T>) -> Option<Vec<T>> {
    let list = crate::agterm::agtermctl(&["window", "list", "--json"])?;
    let mut out = Vec::new();
    for window in crate::agterm::open_window_ids(&list) {
        let Some(tree) = crate::agterm::agtermctl(&["tree", "--json", "--window", &window]) else {
            tracing::debug!(terminal = NAME, window = %window, "no tree for this window; its sessions are not {what} this pass");
            continue;
        };
        let nodes: Vec<Value> = crate::agterm::session_nodes(&tree).cloned().collect();
        out.extend(map(&window, &nodes));
    }
    Some(out)
}

/// Read a session node's two portable handles.
///
/// The one place that knows how an agterm session node maps onto a
/// [`TerminalSession`] — three copies of this had grown, and a terminal that ever
/// exposes a third handle would have been given it by only some of them.
fn session_from_node(node: &Value) -> TerminalSession {
    TerminalSession {
        cwd: node.get("cwd").and_then(Value::as_str).map(str::to_string),
        title: node.get("title").and_then(Value::as_str).map(str::to_string),
    }
}

/// One window's reading: who is selected, what runs there, and what covers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// agterm's own id for the selected session. The key for detecting a *change*
    /// of selection, and deliberately not the dashboard's row id: several agterm
    /// sessions can share one row, and switching between two tabs of the same
    /// project is a real departure from the first.
    pub session_id: String,
    /// The session now on screen.
    pub selected: TerminalSession,
    pub occupant: super::Occupant,
    pub cover: super::Cover,
}

/// Read one window's `tree --json` answer.
///
/// Pure and fixture-pinned: the shape this depends on is agterm's private state,
/// so a schema change should break a test here rather than silently make every
/// session look unattended.
pub fn parse_reading(tree: &Value) -> Option<Reading> {
    let active = crate::agterm::session_nodes(tree).find(|s| s.get("active").and_then(Value::as_bool) == Some(true))?;
    Some(Reading { session_id: active.get("id").and_then(Value::as_str)?.to_string(), selected: session_from_node(active), occupant: facts::occupant(Some(active)), cover: facts::cover(Some(active)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::window_files::{FileRead, WindowFiles};
    use crate::terminals::{person_verdict, Cover, NamedBy, Occupant, Refusal};

    fn tree(active_id: &str, active_title: &str) -> Value {
        let t = serde_json::json!({"workspaces": [
            {"name": "apps", "sessions": [
                {"id": "S1", "cwd": "/p/one", "title": "🟢 one", "active": active_id == "S1", "foreground": ["claude"], "overlay": false},
                {"id": "S2", "cwd": "/p/two", "title": active_title, "active": active_id == "S2", "foreground": ["-zsh"], "overlay": false},
            ]},
        ]});
        serde_json::json!({"ok": true, "result": {"tree": t}})
    }

    /// Trimmed from the live file, so a layout change breaks a test here rather
    /// than silently stopping every departure.
    fn snapshot(version: u64, selected: &str) -> Value {
        serde_json::json!({
            "version": version,
            "selectedSessionID": selected,
            "sidebarMode": "tree",
            "sidebarVisible": true,
            "sessionRecency": [],
            "workspaces": [
                {"id": "W1", "name": "common", "sessions": [
                    {"id": "S1", "cwd": "/p/one", "flagged": false, "isSplit": false},
                    {"id": "S2", "cwd": "/p/two", "flagged": false, "isSplit": false},
                ]},
            ],
        })
    }

    fn seen(id: &str, seen_ms: i64) -> Seen {
        Seen { id: id.into(), seen_ms }
    }

    #[test]
    fn a_snapshot_yields_the_selection_and_every_working_directory() {
        let s = parse_snapshot(&snapshot(1, "S2")).expect("snapshot");
        assert_eq!(s.selected_session_id.as_deref(), Some("S2"));
        assert_eq!(s.cwds.get("S1").map(String::as_str), Some("/p/one"));
        assert_eq!(s.cwds.get("S2").map(String::as_str), Some("/p/two"));
    }

    #[test]
    fn an_unknown_snapshot_version_is_refused_rather_than_guessed_at() {
        // Private, undocumented state with no compatibility promise. Reading a
        // layout we do not know would produce confident wrong departures, which
        // hide finished work; standing down credits no departure until it reads
        // again, and leaves the poll's input observations.
        assert!(parse_snapshot(&snapshot(2, "S2")).is_none());
        assert!(parse_snapshot(&serde_json::json!({"selectedSessionID": "S2"})).is_none(), "no version at all");
    }

    fn left(prev: Option<&Seen>, snap: &Value, written: i64) -> (Option<Seen>, Option<Left>) {
        match step("W1", prev, &snap.to_string(), written) {
            Stepped::Read(next, left) => (next, left),
            Stepped::Unknown(reason) => panic!("refused: {reason}"),
        }
    }

    #[test]
    fn a_changed_selection_departs_the_previous_one_between_its_last_sighting_and_a_debounce_before_the_write() {
        // S1 was confirmed selected at 9 900, after the debounce began.
        let (next, l) = left(Some(&seen("S1", 9_900)), &snapshot(1, "S2"), 10_000);
        assert_eq!(next, Some(seen("S2", 10_000)));
        assert_eq!(l, Some(Left { window: "W1".into(), id: "S1".into(), cwd: Some("/p/one".into()), at_ms: 10_000 - SAVE_DEBOUNCE_MS, switched_ms: 10_000 - SAVE_DEBOUNCE_MS }));
        // Last seen by a write at 9 000, the switch can have been any time since.
        let (_, l) = left(Some(&seen("S1", 9_000)), &snapshot(1, "S2"), 10_000);
        assert_eq!(l.map(|l| (l.at_ms, l.switched_ms)), Some((9_000, 10_000 - SAVE_DEBOUNCE_MS)));
    }

    #[test]
    fn a_switch_saved_late_is_no_earlier_than_the_previous_write() {
        // agterm was behind a browser. One click on its sidebar at 1 000 brought
        // it forward and switched S1 to S2, and further changes kept the save
        // back until 20 000. The previous write, at 900, still showed S1, so the
        // switch is no earlier than that, whatever the late write says.
        let (_, l) = left(Some(&seen("S1", 900)), &snapshot(1, "S2"), 20_000);
        let l = l.expect("a departure");
        assert_eq!((l.at_ms, l.switched_ms), (900, 20_000 - SAVE_DEBOUNCE_MS));
        let o = departure(&l, Some(&tree("S2", "🔵 two")), Front::Yes, Since::At(1_000), LastInput::At(19_650), 20_000);
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::ActivatedBySwitch), "the click that brought agterm forward");
    }

    #[test]
    fn a_rewrite_with_the_same_selection_or_a_first_sighting_departs_nothing() {
        assert_eq!(left(Some(&seen("S2", 9_000)), &snapshot(1, "S2"), 10_000), (Some(seen("S2", 10_000)), None), "the newest write is the newest sighting");
        assert_eq!(left(None, &snapshot(1, "S2"), 10_000), (Some(seen("S2", 10_000)), None));
    }

    #[test]
    fn a_snapshot_with_no_selection_keeps_the_previous_one() {
        let mut s = snapshot(1, "S2");
        s.as_object_mut().unwrap().remove("selectedSessionID");
        assert_eq!(left(Some(&seen("S1", 9_000)), &s, 10_000), (Some(seen("S1", 9_000)), None));
    }

    #[test]
    fn an_unknown_snapshot_stands_the_window_down() {
        assert!(matches!(step("W1", Some(&seen("S1", 0)), &snapshot(2, "S2").to_string(), 10_000), Stepped::Unknown(_)));
        assert!(matches!(step("W1", Some(&seen("S1", 0)), "not json", 10_000), Stepped::Unknown(_)));
    }

    #[test]
    fn a_snapshot_carries_no_title_which_is_why_a_departure_still_costs_a_tree_call() {
        // Pins the fact the design rests on: `resolve_row` prefers the title
        // because a `cd`-ed session's cwd derives another row's id, and the file
        // has no title to offer.
        let s = snapshot(1, "S2");
        let sessions = s["workspaces"][0]["sessions"].as_array().expect("sessions");
        assert!(sessions.iter().all(|n| n.get("title").is_none()));
    }

    /// Window files holding `W1` tracked as `S1`, last seen at 1 000.
    fn tracked_s1() -> WindowFiles<Seen> {
        let mut files = WindowFiles::new(NAME);
        let read = vec![FileRead { window: "W1".into(), contents: crate::terminals::window_files::Contents::Read(snapshot(1, "S1").to_string(), 1_000) }];
        files.pass(read, true, |window, prev, text, written_ms| step(window, prev, text, written_ms));
        files
    }

    #[test]
    fn a_selection_confirmed_after_an_activation_places_the_next_switch_after_it() {
        // agterm came forward at 5 000 and still had S1 selected at 5 501, so a
        // switch saved later is no earlier than that, and is not taken for the
        // click that brought agterm forward.
        let mut files = tracked_s1();
        assert_eq!(confirm_live(&mut files, "W1", Some("S1"), 5_501), "selection_confirmed");
        assert_eq!(files.tracked()["W1"], seen("S1", 5_501));
        let (_, l) = left(files.tracked().get("W1"), &snapshot(1, "S2"), 30_000);
        let o = departure(&l.expect("a departure"), Some(&tree("S2", "🔵 two")), Front::Yes, Since::At(5_000), LastInput::At(29_650), 30_000);
        assert_eq!(person_verdict(&o, NamedBy::Title), Ok(()));
        assert_eq!(confirm_live(&mut files, "W1", Some("S1"), 2_000), "selection_confirmed");
        assert_eq!(files.tracked()["W1"].seen_ms, 5_501, "an older confirmation moves nothing back");
    }

    #[test]
    fn a_window_brought_forward_inside_agterm_has_its_next_switch_placed_after_the_poll_that_read_it() {
        // agterm came forward at 5 000 with W1 active, so only W1 was confirmed.
        // The user then clicked into W2, which raises no activation, and later
        // switched its session; W2's tracking was last written at 1 000.
        let switch_in_w2 = |files: &WindowFiles<Seen>| {
            let Stepped::Read(_, Some(l)) = step("W2", files.tracked().get("W2"), &snapshot(1, "S2").to_string(), 30_000) else { panic!("a departure") };
            let front = facts::front(facts::AppFront::Agterm, &facts::Windows::Listed { active: Some("W2".into()) }, "W2");
            person_verdict(&departure(&l, Some(&tree("S2", "🔵 two")), front, Since::At(5_000), LastInput::At(29_650), 30_000), NamedBy::Title)
        };
        let mut files = WindowFiles::new(NAME);
        let read = vec![FileRead { window: "W2".into(), contents: crate::terminals::window_files::Contents::Read(snapshot(1, "S1").to_string(), 1_000) }];
        files.pass(read, true, |window, prev, text, written_ms| step(window, prev, text, written_ms));
        assert_eq!(switch_in_w2(&files), Err(Refusal::ActivatedBySwitch), "with nothing but the write, the switch can have been the activating click");
        // A poll tick at 20 000 read W2 with S1 still selected.
        let mut selections = crate::terminals::SelectionClock::default();
        let reading = parse_reading(&tree("S1", "🟢 one")).expect("reading");
        assert_eq!(take_reading(&mut files, &mut selections, "W2", &reading, 20_000, 20_050), (20_050, "selection_confirmed"));
        assert_eq!(switch_in_w2(&files), Ok(()), "the switch came after the poll");
        // A poll that finds another selection than the file's forgets the window,
        // so the save that catches up departs nothing.
        let other = parse_reading(&tree("S2", "🔵 two")).expect("reading");
        assert_eq!(take_reading(&mut files, &mut selections, "W2", &other, 25_000, 25_050), (25_050, "file_behind_screen"));
        assert!(!files.tracked().contains_key("W2"));
    }

    #[test]
    fn another_selection_confirmed_after_an_activation_forgets_the_window() {
        // The click that brought agterm forward also switched to S2, and its save
        // has not arrived: when it does, it is a first sighting and departs
        // nothing.
        let mut files = tracked_s1();
        assert_eq!(confirm_live(&mut files, "W1", Some("S2"), 5_501), "file_behind_screen");
        assert!(!files.tracked().contains_key("W1"));
        assert_eq!(confirm_live(&mut files, "W1", Some("S2"), 5_502), "untracked");
        let mut files = tracked_s1();
        assert_eq!(confirm_live(&mut files, "W1", None, 5_501), "no_selection");
        assert_eq!(files.tracked()["W1"], seen("S1", 1_000));
    }

    #[test]
    fn a_departure_is_named_and_occupied_from_the_tree_of_its_window() {
        let l = Left { window: "W1".into(), id: "S1".into(), cwd: Some("/p/one".into()), at_ms: 9_000, switched_ms: 9_700 };
        let o = departure(&l, Some(&tree("S2", "🔵 two")), Front::Yes, Since::At(1_000), LastInput::At(9_650), 10_000);
        assert_eq!(o.session, TerminalSession { cwd: Some("/p/one".into()), title: Some("🟢 one".into()) });
        assert_eq!((o.occupant, o.naming, o.at_ms), (Occupant::Agent, Naming::OwnTitle, 9_000));
        // The desktop's clock, which counts the click on the sidebar that made
        // the switch.
        assert_eq!(o.kind, ObservationKind::Departed(Switch::Placed { latest_ms: 9_700, front: Front::Yes, front_since: Since::At(1_000), last_input: LastInput::At(9_650) }));
    }

    #[test]
    fn a_departure_the_tree_cannot_name_falls_back_to_the_snapshot_directory_and_says_so() {
        let l = Left { window: "W1".into(), id: "GONE".into(), cwd: Some("/p/gone".into()), at_ms: 9_700, switched_ms: 9_700 };
        let o = departure(&l, None, Front::Yes, Since::At(1_000), LastInput::At(5_000), 10_000);
        assert_eq!(o.session, TerminalSession { cwd: Some("/p/gone".into()), title: None });
        assert_eq!(o.occupant, Occupant::Unknown("not_in_tree"), "named by its directory alone, so the verdict will ask, and refuse");
        assert_eq!(o.kind, ObservationKind::Departed(Switch::Placed { latest_ms: 9_700, front: Front::Yes, front_since: Since::At(1_000), last_input: LastInput::At(5_000) }));
    }

    #[test]
    fn a_departure_read_long_after_its_write_places_it_has_its_facts_unknown() {
        // Input since the switch would otherwise pass for input at it.
        let l = Left { window: "W1".into(), id: "S1".into(), cwd: Some("/p/one".into()), at_ms: 9_700, switched_ms: 9_700 };
        let o = departure(&l, None, Front::Yes, Since::At(1_000), LastInput::At(30_000), 9_700 + FACTS_ALLOWANCE_MS + 1);
        assert_eq!(o.kind, ObservationKind::Departed(Switch::Placed { latest_ms: 9_700, front: Front::Unknown("read_after_switch"), front_since: Since::At(1_000), last_input: LastInput::Unknown }));
    }

    #[test]
    fn a_departure_from_a_tab_its_agent_has_left_is_refused_under_the_title_it_left_behind() {
        // The agent exited and its title stayed on the tab, whose shell is now
        // what is in front of it.
        let l = Left { window: "W1".into(), id: "S2".into(), cwd: Some("/p/two".into()), at_ms: 9_000, switched_ms: 9_700 };
        let o = departure(&l, Some(&tree("S1", "🟢 two")), Front::Yes, Since::At(1_000), LastInput::At(9_650), 10_000);
        assert_eq!((o.session.title.as_deref(), o.occupant), (Some("🟢 two"), Occupant::Shell));
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::BareShell));
    }

    #[test]
    fn a_reading_names_the_selected_session_what_runs_in_it_and_what_covers_it() {
        let r = parse_reading(&tree("S2", "🔵 two")).expect("reading");
        assert_eq!(r.session_id, "S2");
        assert_eq!(r.selected.title.as_deref(), Some("🔵 two"));
        assert_eq!(r.selected.cwd.as_deref(), Some("/p/two"));
        assert_eq!((r.occupant, r.cover), (Occupant::Shell, Cover::Clear));
        let silent = serde_json::json!({"ok": true, "result": {"tree": {"workspaces": [{"sessions": [{"id": "S1", "title": "🟢 one", "active": true, "foreground": ["claude"]}]}]}}});
        assert_eq!(parse_reading(&silent).map(|r| r.cover), Some(Cover::Clear), "a node that does not mention the overlay at all");
    }

    #[test]
    fn an_unrecognized_tree_yields_nothing_rather_than_a_guess() {
        assert!(parse_reading(&serde_json::json!({"ok": true})).is_none());
        let none_active = serde_json::json!({"ok": true, "result": {"tree": {"workspaces": [
            {"name": "apps", "sessions": [{"id": "S1", "cwd": "/p/one", "active": false}]},
        ]}}});
        assert!(parse_reading(&none_active).is_none(), "agterm open but the user is nowhere");
    }

    #[test]
    fn input_needs_agterm_in_front_since_before_it() {
        // The agent's window, read beside a browser: the desktop's input went to
        // the browser.
        let reading = parse_reading(&tree("S1", "🟢 one")).expect("reading");
        let listed = facts::Windows::Listed { active: Some("W1".into()) };
        let behind = input_observation(&reading, facts::front(facts::AppFront::Other, &listed, "W1"), Since::At(0), 0, 60_000);
        assert_eq!(person_verdict(&behind, NamedBy::Title), Err(Refusal::NotInFront));
        let in_front = input_observation(&reading, facts::front(facts::AppFront::Agterm, &listed, "W1"), Since::At(0), 0, 60_000);
        assert_eq!(person_verdict(&in_front, NamedBy::Title), Ok(()));
    }

    #[test]
    fn with_several_windows_open_the_active_one_is_credited_from_agterms_activation() {
        let reading = parse_reading(&tree("S1", "🟢 one")).expect("reading");
        let two = facts::Windows::Listed { active: Some("W2".into()) };
        let input = input_observation(&reading, facts::front(facts::AppFront::Agterm, &two, "W2"), Since::At(0), 0, 60_000);
        assert_eq!(person_verdict(&input, NamedBy::Title), Ok(()), "input to the active window");
        let behind = input_observation(&reading, facts::front(facts::AppFront::Agterm, &two, "W1"), Since::At(0), 0, 60_000);
        assert_eq!(person_verdict(&behind, NamedBy::Title), Err(Refusal::NotInFront), "input is not credited to the window behind");
        let l = Left { window: "W2".into(), id: "S1".into(), cwd: Some("/p/one".into()), at_ms: 9_000, switched_ms: 9_700 };
        let switched = departure(&l, Some(&tree("S2", "🔵 two")), facts::front(facts::AppFront::Agterm, &two, "W2"), Since::At(1_000), LastInput::At(9_650), 10_000);
        assert_eq!(person_verdict(&switched, NamedBy::Title), Ok(()), "a switch in the active window");
    }

    #[test]
    fn the_click_that_brought_agterm_forward_is_refused_however_late_its_activation_was_recorded() {
        // The activation's notification reached this process 150 ms after the
        // click that made it, and the click is the newest desktop input.
        let reading = parse_reading(&tree("S1", "🟢 one")).expect("reading");
        let o = input_observation(&reading, Front::Yes, Since::At(10_150), 0, 10_000);
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::InputBeforeFront));
    }

    /// The shared rules, exercised through the key *this* adapter uses — agterm's
    /// own session id under plain equality. `terminals::departure_stamp` owns
    /// them; this pins that the call site passes them what it means to.
    fn stamp(previous: Option<&str>, current: &str, last: Option<i64>, now: i64) -> Option<i64> {
        super::super::departure_stamp(previous, current, |a: &str, b: &str| a == b, last, now)
    }

    #[test]
    fn staying_on_one_tab_is_not_a_departure() {
        assert_eq!(stamp(Some("S2"), "S2", Some(1_000), 6_000), None);
    }

    #[test]
    fn the_first_sight_of_a_window_is_not_a_departure() {
        // At startup every selection is new to us and there is no earlier session
        // to have left; calling it one would mark whatever is on screen as read.
        assert_eq!(stamp(None, "S2", Some(1_000), 6_000), None);
    }

    #[test]
    fn two_agterm_tabs_of_one_project_are_two_selections() {
        // Why the key is agterm's session id and not a row id: several sessions
        // can share one dashboard row, and switching between them is a real
        // departure from the first.
        assert_eq!(stamp(Some("S1"), "S2", Some(1_000), 6_000), Some(1_000));
    }
}
