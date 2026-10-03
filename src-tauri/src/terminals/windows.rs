//! The Windows adapter: what Windows Terminal shows, and what the console holds.
//!
//! Three questions with three different oracles, and the split is not tidiness —
//! it is that on Windows they genuinely live in different places. A fourth read,
//! the owner walk behind `attached_surface`, answers which window renders a
//! console this process has just written to.
//!
//! - **Which session is on screen** is Windows Terminal's *window title*. WT
//!   publishes the active tab's title as the window caption, and that string is
//!   the one this dashboard itself wrote (`terminal_title::build_title`), so a
//!   `GetWindowTextW` says which row the user is looking at. It is the exact
//!   analogue of agterm's `selectedSessionID`, and it is all WT offers: the
//!   `wt.exe` CLI is fire-and-forget with no query, and nothing reaches disk when
//!   a tab is switched, so there is no snapshot to watch the way the macOS
//!   adapter watches agterm's.
//! - **What a session is showing** is its *console object*, read per pid by
//!   `terminal_title::read_title`. On Windows `push_title` writes
//!   `SetConsoleTitleW`; the terminal only renders that. So reading the console
//!   back is this dashboard reading its own last published record, one per
//!   session, where the window title can only ever report the one tab in front.
//!
//! **The primary signal is departure**, as on macOS: the active tab going from S
//! to T means the user *left* S, and leaving is the moment you are done with what
//! was on screen. Arriving marks nothing.
//!
//! **The watch is the only source of credited departures; the poll contributes
//! input.** Sampling a level to catch an edge misses any visit that begins and
//! ends between two polls, and measured on this machine real visits run 1.7 and
//! 2.2 seconds against a 30-second tick. A switch the poll does find happened at
//! an instant it cannot name, so the verdict refuses it, and with the watch's
//! hooks lost no departure is credited at all. [`WindowsAdapter::watch`] takes
//! the edge directly from
//! `SetWinEventHook(EVENT_OBJECT_NAMECHANGE)`, which arrives in under 100 ms with
//! no debounce to coalesce a quick in-and-out — so the residual blind window the
//! macOS adapter still has does not exist here.
//!
//! **The hook is scoped to Windows Terminal's process**, which is what makes it
//! affordable: measured, a global hook delivered 986 events in 34 s (792 of them
//! explorer's tree view) against 6 in 22 s scoped to the one pid, filtered by the
//! OS before they reach this process.
//!
//! Two hazards shape everything below, both of them ways to mark a row read that
//! nobody read — the one direction this must never fail in.
//!
//! - **A title changes for two different reasons.** The user switched tabs, or we
//!   rewrote the tab already on screen (a glyph moving, the context suffix
//!   ticking, a drift badge appearing). Both produce one `NAMECHANGE` on one
//!   window. `terminal_title::same_row` is the discriminator, and it is asked
//!   about the two *titles* because WT hands out no session handle to key on.
//! - **A tab switch is not always a person.** `wt.exe focus-tab` from a script
//!   switches tabs with nobody watching. Every human switch measured had the
//!   window in the foreground with input under 50 ms old — a switch *is* an input
//!   event — while script-driven ones did not. This adapter does not judge that:
//!   the watch reads the foreground, since when that window has held it, and the
//!   input clock at the event's instant, and `crate::attention` applies
//!   [`super::person_verdict`] to them as it does to every terminal's. The poll
//!   learns of a switch some time after the fact, when the foreground has moved
//!   on, so it reports the switch as unplaced and the verdict refuses it, which is
//!   the honest difference between knowing an instant and knowing an interval.
//!   Since when a window has held the foreground comes from the foreground hook,
//!   and is trusted only once the hooks have delivered an event: see
//!   [`ForegroundRecord`].
//!
//! Nothing Windows Terminal exposes says what runs in a pane, so every
//! observation reports the occupant unknown. The verdict does not need it where
//! an observation is named by the console title this dashboard wrote onto the
//! agent's own console, which a shell beside it does not carry. **A caption is
//! not that title until it is checked**: it is the tab's *displayed* name, and a
//! tab renamed by hand keeps showing whatever it was showing when it was renamed,
//! over whatever the console now holds — an exited agent's last glyph over a
//! plain shell. So each caption is checked against the console titles of the
//! panes behind it, read through UI Automation while that tab is the one in
//! front, and only a caption one of them carries is reported as the session's own
//! title. Any other is a displayed name, which keeps the verdict asking what runs
//! in the pane, and with nothing to say that, refusing. A departure is judged by
//! the naming read when its tab came forward, which a tab pinned while it stays in
//! front would keep; so a caption whose row has had a title written since that
//! read is reported as a displayed name too ([`naming_at_departure`]), since the
//! write would have moved the caption had the tab still followed its console.
//!
//! The verdict's activation rule refuses the click that both focuses a window and
//! switches its tab. Nothing here says whether the window was covered before that
//! click, so a switch made straight from reading an unfocused but visible window,
//! on a second monitor or beside the focused one, is refused too; see
//! [`super::ACTIVATION_MS`].

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Manager};

use super::{Cover, Front, FrontReading, InputFacts, LastInput, Naming, Observation, ObservationKind, Occupant, Selection, SelectionClock, Since, Switch, TerminalAdapter, TerminalSession};

/// The slug the decision log carries. Deliberately the platform and not
/// `"windows_terminal"`: attention really is Windows Terminal's, but
/// [`WindowsAdapter::sessions`] reads console objects and so answers for a VS
/// Code terminal or a bare conhost too, and a slug naming WT would be false on
/// every `restore_scan` line for one of those.
pub(super) const NAME: &str = "windows";

/// Windows Terminal's top-level window class. Its other window — an invisible
/// `Windows Terminal <hex>` — is the monarch, which is why visibility is checked
/// as well.
const TERMINAL_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";

/// What to tell a user whose tab has stopped following its session.
///
/// It names one cause, because since `stale_check` became the flag's only writer
/// only one cause can raise it. That check needs the displayed name to differ
/// from the pane's console title, and a leftover tab whose session merely exited
/// shows the two as one string by construction — so telling the user it might be
/// that would send them looking for something the detector cannot have seen.
///
/// Named rather than written inline so the contract test in the parent module
/// can hold it to [`super::TerminalAdapter::stale_remedy`]'s splicing rules.
/// That is a constant a reader benefits from either way, not a surface added
/// for a test: an adapter needs an `AppHandle` to construct, so a test cannot
/// reach the method.
pub(super) const STALE_REMEDY: &str = "right-click the tab and choose \"Reset tab title\". A double-click on a tab opens the renamer, which pins it to whatever it was showing at the time, so this happens with nothing typed";

/// The opaque surface key for one terminal window, per [`super::FrontReading`].
///
/// One function so no two sides can drift: `front_readings` mints these, the
/// caption watch mints them for the window it saw, and `attached_surface` mints
/// one when it attributes a session's console to the window rendering it.
/// A caller comparing a key built one way against a key built another would
/// silently match nothing — and since every reader treats an unmatched key as
/// "do not know", it would fail by going quiet.
fn surface_key(hwnd: isize) -> String {
    format!("hwnd:{hwnd}")
}

/// How far before the event the switch itself is assumed to have happened.
/// Measured under 100 ms; erring early only leaves a row showing, where erring
/// late would credit a departure with content that arrived after the user had
/// gone. The counterpart of agterm's `SAVE_DEBOUNCE_MS`.
const SWITCH_LATENCY_MS: i64 = 250;

/// How often the hook thread re-checks which process is Windows Terminal, so a
/// terminal that starts after the dashboard — or restarts — is picked up without
/// a poll of its own.
const PID_RECHECK_MS: u32 = 5_000;

/// Where the hook callback puts what it saw. A `WINEVENTPROC` is a bare
/// `extern "system" fn` with nowhere to carry state, so the channel is a static
/// and every judgment happens on the receiving side.
static EVENTS: OnceLock<Sender<Raw>> = OnceLock::new();

/// What the hook thread hands the consumer.
enum Raw {
    /// Hooks were installed for a new terminal process, or the last one went
    /// away: whatever was recorded about the foreground before no longer applies.
    /// `front` is the terminal window already in front then, posted only when
    /// the foreground hook itself is in place, since without it nothing would
    /// ever renew or end that record.
    Hooked { front: Option<RawEvent> },
    /// An event the hooks delivered.
    Event(RawEvent),
}

/// One raw window event, with the two facts that are only true *at its instant*
/// read there rather than whenever the consumer gets to it.
struct RawEvent {
    event: u32,
    hwnd: isize,
    title: String,
    foreground: isize,
    idle_ms: Option<u64>,
    at_ms: i64,
}

/// Which terminal window holds the foreground and since when — the only thing
/// [`EVENT_SYSTEM_FOREGROUND`] is used for. It deliberately emits no observation
/// of its own: focus leaves a terminal for reasons that are not a person leaving
/// a tab (a UAC prompt, a toast, this app's own history window), and marking on
/// those would hide finished work.
type Foreground = Arc<Mutex<ForegroundRecord>>;

/// The last terminal window the foreground events saw take the foreground, and
/// whether the hooks have shown they deliver at all.
///
/// The second half is what makes the first trustworthy. The record starts from
/// the window in front when the hooks were installed, because no event says so
/// until focus moves, and a record nothing renews is only true for as long as
/// events would have arrived to end it. An elevated Windows Terminal accepts the
/// hooks, returns handles and delivers nothing to this process, so its starting
/// record would stand for the life of the process while the user typed in other
/// programs and came back. Until one event has arrived from the hooked process,
/// the record answers that nothing is recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ForegroundRecord {
    held: Option<(isize, i64)>,
    delivered: bool,
}

impl ForegroundRecord {
    /// Since when `hwnd` has held the foreground, as far as this record can say.
    fn since(&self, hwnd: isize) -> Since {
        if self.delivered {
            front_since(self.held, hwnd)
        } else {
            Since::Unrecorded
        }
    }
}

/// When each window's active tab began, written by the watch on every title
/// change and by the poll on every reading, keyed by window handle and compared
/// by [`same_tab`].
type Selections = Arc<Mutex<SelectionClock<isize>>>;

/// Whether two captions of one window are one selection, for
/// [`SelectionClock`]: only when they are the same string.
///
/// Deliberately stricter than [`crate::terminal_title::same_row`], which the
/// departures use and which errs toward "the same row" — `🟢 bga` and
/// `🟢 bga assistant` share a label — because the two errors cost opposite
/// things. A departure missed leaves a row showing; a selection start kept from
/// the previous tab is earlier than the truth, and input made in that tab, or the
/// click that arrived, would pass for reading the next one. Under exact equality
/// this dashboard's own rewrite of the tab in front restarts the selection, which
/// refuses input only within [`super::SWITCH_RELEASE_MS`] of that write.
fn same_tab(a: &str, b: &str) -> bool {
    a == b
}

/// Why every observation here reports the occupant unknown.
const OCCUPANT_UNREPORTED: &str = "not_reported";

pub struct WindowsAdapter {
    app: AppHandle,
    /// Per window, the last title seen on its active tab — the whole departure
    /// signal for the poll. The **raw title**, never a name derived from it, so
    /// absence has exactly one meaning: this window has not been seen before. A
    /// window showing something we did not write is a state, not a missing
    /// reading, and storing it as one is what lets leaving a tracked tab for a
    /// plain shell count as the departure it is.
    last_title: HashMap<isize, String>,
    last_poll_at: Option<i64>,
    foreground: Foreground,
    selections: Selections,
}

impl WindowsAdapter {
    pub fn new(app: AppHandle) -> Self {
        Self { app, last_title: HashMap::new(), last_poll_at: None, foreground: Arc::new(Mutex::new(ForegroundRecord::default())), selections: Arc::new(Mutex::new(SelectionClock::default())) }
    }
}

impl TerminalAdapter for WindowsAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Every live session's console title, paired with the working directory the
    /// session registry holds for it.
    ///
    /// One entry per *record*, not per row: two sessions sharing a directory
    /// yield two consoles, which lets `session_restore::title_status` do what it
    /// already does on macOS with two tabs — restore when they agree, refuse when
    /// they disagree — instead of this module picking one of them.
    ///
    /// The `None` contract is the caller's retry gate, so it is drawn where the
    /// caller needs it: `None` when the registry could not be read, and equally
    /// when it named live sessions and not one console would answer, since that
    /// is a failure to look rather than a finding. `Some(vec![])` says only that
    /// nothing is running.
    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        let registry = self.app.try_state::<crate::session_registry::SessionRegistry>()?;
        let records = registry.live_records(crate::commands::now_ms())?;
        let tabs: Vec<TerminalSession> = records.into_iter().map(|(pid, cwd)| TerminalSession { cwd: Some(cwd), title: crate::terminal_title::read_title(pid) }).collect();
        if !tabs.is_empty() && tabs.iter().all(|t| t.title.is_none()) {
            tracing::debug!(decision = "restore_scan", terminal = NAME, outcome = "no_console_answered", sessions = tabs.len(), "live sessions, but not one console title could be read");
            return None;
        }
        Some(tabs)
    }

    fn poll(&mut self, now_ms: i64) -> Vec<Observation> {
        let mut out = Vec::new();
        if !titles_enabled(&self.app) {
            // Every observation this adapter makes is named by a title this
            // dashboard wrote, so with titling off the sensor has no way to name
            // a row at all. Saying so beats logging `no_target` forever, which
            // reads like a sensor that ran and found nothing.
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome = "titles_disabled", "terminal titles are off, so no observation can name a row");
            return out;
        }
        let foreground_now = unsafe { GetForegroundWindow() };
        let record = *self.foreground.lock().unwrap();
        let idle_ms = crate::idle::idle_ms();
        let windows = terminal_windows();
        let mut selections = self.selections.lock().unwrap();
        selections.retain(|hwnd| windows.iter().any(|(w, _)| w == hwnd));
        for (hwnd, title) in windows {
            let previous = self.last_title.insert(hwnd, title.clone());
            let selected_since = selections.note(hwnd, &title, same_tab, now_ms);
            let outcome = match super::departure_stamp(previous.as_deref(), &title, crate::terminal_title::same_row, self.last_poll_at, now_ms) {
                Some(at_ms) => {
                    // The row the user *left*, which is the one they were
                    // reading — not the one they arrived at. Its tab is no
                    // longer in front, so its panes cannot be read for its
                    // naming; the verdict refuses an unplaced switch first
                    // anyway.
                    out.push(observed(previous.as_deref(), Naming::DisplayedName, at_ms, ObservationKind::Departed(Switch::Unplaced)));
                    "switched"
                }
                None if previous.is_none() => "first_sight",
                None => "same_row",
            };
            // The desktop's last input, offered for every window: which one it
            // reached is the verdict's to decide from the foreground. The tab's
            // naming is read only for the window in front, since the verdict
            // refuses every other as `not_in_front` before it asks.
            if let Some(idle) = idle_ms {
                let front = front_of(hwnd, foreground_now);
                let facts = InputFacts { front, front_since: record.since(hwnd), selection: Selection::Live, selected_since: Since::At(selected_since), cover: Cover::Clear };
                out.push(observed(Some(&title), naming_where(front == Front::Yes, hwnd, &title), now_ms - idle as i64, ObservationKind::Input(facts)));
            }
            // Logged on every window every pass, including the ones observing
            // nothing: a sensor whose success and whose total failure are both
            // silent cannot be told apart from one that never ran.
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome, hwnd, title, has_idle = idle_ms.is_some(), "windows terminal poll");
        }
        self.last_poll_at = Some(now_ms);
        out
    }

    /// Windows Terminal offers no way to undo a rename from outside, so the
    /// remedy names the one action only the user can take.
    fn stale_remedy(&self) -> &'static str {
        STALE_REMEDY
    }

    /// The terminal window rendering the console this process is attached to.
    ///
    /// Three outcomes, and the difference between the last two matters. A handle
    /// of zero is a `CREATE_NO_WINDOW` console — a hook process, never a tab. A
    /// window whose root owner is *itself* is a console nothing owns: a bare
    /// `conhost`, or a ConPTY whose host never claimed it, which is how a session
    /// in another terminal presents. Only a root owner of Windows Terminal's own
    /// class is a tab this adapter can reason about, so the test is
    /// [`is_terminal_window`] and **never** "an owner exists" — any other
    /// terminal that sets an owner would otherwise be taken for this one.
    ///
    /// The caller has already attached to `pid`'s console, so this reads that
    /// console rather than looking the pid up: `GetConsoleWindow` answers for the
    /// attachment, which is what makes the result per-session rather than
    /// per-process and what confines the call to the inside of a write.
    ///
    /// Measured on Windows 11: a ConPTY console reports a real (invisible, 0x0)
    /// window of class `PseudoConsoleWindow`, and the consoles reporting no window
    /// at all are exactly the hook-side `CREATE_NO_WINDOW` ones. The owner link is
    /// cross-process — the pseudo-console window belongs to `OpenConsole.exe`, not
    /// to `WindowsTerminal.exe` — so it was measured end to end on 2026-09-04
    /// against all six live sessions: every one resolved to the same visible
    /// `CASCADIA_HOSTING_WINDOW_CLASS` window, and to the same handle
    /// `front_readings` enumerates for it.
    ///
    /// Four kernel-side calls, no message send, no lock: cheap enough for the
    /// caller's attach lock, per the trait's requirement.
    fn attached_surface(&self, pid: u32) -> Option<String> {
        // The attachment names the console; the pid is what the caller attached.
        let _ = pid;
        let console = unsafe { GetConsoleWindow() };
        if console == 0 {
            return None;
        }
        let owner = unsafe { GetAncestor(console, GA_ROOTOWNER) };
        if owner == 0 || owner == console {
            return None;
        }
        is_terminal_window(owner).then(|| surface_key(owner))
    }

    /// UI Automation is COM, so the reading thread must join an apartment
    /// before [`front_readings`](TerminalAdapter::front_readings) can be
    /// called on it.
    fn prepare_reader(&self) {
        super::wt_tabs::init_apartment();
    }

    /// One reading per visible Windows Terminal window: the name on the tab in
    /// front, and the console titles of the panes realized behind it.
    ///
    /// The two sides come from two different places, which is the point. The
    /// tab's name is what Windows Terminal *renders*, and a rename overwrites it.
    /// The pane's UIA `HelpText` is `ControlCore::Title()`, the console title
    /// itself, which a rename does not touch. Measured live 2026-09-03:
    /// `shown=ttt` against `real=✋ what-is-next [78%]`.
    ///
    /// **Both come from the one UIA pass, and the window caption is deliberately
    /// not used** even though this adapter already reads it for attention. Under
    /// `showTerminalTitleInTitlebar: false` every caption is the literal string
    /// `Windows Terminal` while each tab keeps its own name, so a caption-based
    /// comparison would report every session on that machine as stale at once.
    /// See `wt_tabs` for the second reason, which is that two APIs put the two
    /// sides of the comparison at different instants.
    ///
    /// Only the tab in front of each window answers, and that is structural
    /// rather than a shortfall: XAML's `TabView` realizes one `ContentPresenter`,
    /// so the pane search returns the panes of the selected tab and of no other. It
    /// is also the right limit — a stale glyph misleads precisely while its tab
    /// is on screen.
    fn front_readings(&self) -> Option<Vec<FrontReading>> {
        // Always `Some`: enumerating this machine's terminal windows is a
        // local call with no way to fail into "could not be asked". A window
        // whose *contents* could not be read is a per-surface abstention
        // below, which is the finer answer and the one the rule needs.
        Some(
            terminal_windows()
                .into_iter()
                .map(|(hwnd, _caption)| {
                    // No caption fallback. It could never reach a verdict — a failed
                    // pass leaves `sessions` at `None`, which abstains — but it would
                    // hand the rule a *non-empty* `shown`, so a window whose read
                    // failed and whose caption was readable would log its abstention
                    // under whichever reason the rule tested first. "Could not look"
                    // must never wear the name of "showed nothing".
                    let read = super::wt_tabs::read_surface(hwnd);
                    FrontReading {
                        // Opaque per the seam's contract: the caller compares these
                        // for equality and never reads the number back out.
                        surface: surface_key(hwnd),
                        shown: read.as_ref().map_or_else(String::new, |r| r.shown.clone()),
                        sessions: read.map(|r| r.panes),
                    }
                })
                .collect(),
        )
    }

    /// Report a departure the moment the active tab's title changes, rather than
    /// at the next tick.
    ///
    /// Two threads, because the callback and the judgment have opposite
    /// requirements. A `WINEVENTPROC` must return fast or it drains USER
    /// resources for the whole desktop, so it does nothing but read the three
    /// instant-sensitive facts and post them; the consumer owns the diff map, the
    /// gate and the sink.
    fn watch(&self, sink: Sender<Observation>) {
        let (tx, rx) = std::sync::mpsc::channel();
        if EVENTS.set(tx).is_err() {
            tracing::warn!(terminal = NAME, "the selection watcher is already running");
            return;
        }
        let app = self.app.clone();
        let foreground = self.foreground.clone();
        let selections = self.selections.clone();
        std::thread::spawn(move || consume(&app, &foreground, &selections, &rx, &sink));
        std::thread::spawn(pump);
    }
}

/// Name a session the way the seam names one. `cwd` is always `None` on Windows:
/// neither a window title nor a `NAMECHANGE` carries a working directory, so
/// `attention::resolve_row` resolves these by title alone — which is the
/// resolution it prefers anyway.
fn named(title: Option<&str>) -> TerminalSession {
    TerminalSession { cwd: None, title: title.map(str::to_string) }
}

/// An observation of the tab titled `title`, carrying what is true of every
/// Windows Terminal observation: nothing says what runs in the pane, and no pane
/// is ever drawn over — Windows Terminal's palette and search box leave the
/// pane's output on screen, and its settings open as a tab of their own, which is
/// a different title. `naming` is whether the title was confirmed as the pane's
/// own, by [`naming_where`] while that tab was in front.
fn observed(title: Option<&str>, naming: Naming, at_ms: i64, kind: ObservationKind) -> Observation {
    Observation { terminal: NAME, session: named(title), at_ms, kind, occupant: Occupant::Unknown(OCCUPANT_UNREPORTED), naming }
}

/// How a caption names its tab's session: as the session's own title when one of
/// the panes behind it carries exactly that console title, which this dashboard
/// writes only onto the agent's console, and as a displayed name otherwise —
/// including when the panes could not be read, since that is a failure to look.
fn naming_from(caption: &str, panes: Option<&[Option<String>]>) -> Naming {
    if panes.is_some_and(|panes| panes.iter().any(|pane| pane.as_deref() == Some(caption))) {
        Naming::OwnTitle
    } else {
        Naming::DisplayedName
    }
}

/// [`naming_from`] for window `hwnd`, read now where `worth_reading`, and a
/// displayed name otherwise. Only meaningful while the tab carrying `caption` is
/// the one in front, since only that tab's panes are realized, so the watch reads
/// it on the caption change that brought the tab forward and keeps it for the
/// departure from it. One cross-process UI Automation pass, ~5 ms warm and ~95
/// ms cold, which is why every caller says when the answer can matter: the watch
/// while titling is on, since otherwise nothing names a row, and the poll for the
/// window in front, since the verdict refuses input to any other before it asks.
fn naming_where(worth_reading: bool, hwnd: isize, caption: &str) -> Naming {
    if !worth_reading {
        return Naming::DisplayedName;
    }
    join_apartment();
    naming_from(caption, super::wt_tabs::read_surface(hwnd).as_ref().map(|s| s.panes.as_slice()))
}

/// The naming a departure from a caption read as the session's own title at
/// `read_ms` still has, given that the newest title this dashboard wrote for a
/// row that caption names changed at `changed_ms`.
///
/// A caption is re-read on every change, and a write to the console behind it
/// changes it, so a title that moved after the read moved somewhere this caption
/// did not follow: a tab pinned by hand, or a console that is no longer the one
/// the row's titles go to. Either way the read no longer says the session behind
/// the caption is the one the row describes, and the caption is only a displayed
/// name. Erring toward a move only refuses more: a row whose label shares a word
/// with another's counts the other's writes too.
fn naming_at_departure(naming: Naming, read_ms: i64, changed_ms: Option<i64>) -> Naming {
    match (naming, changed_ms) {
        (Naming::OwnTitle, Some(changed)) if changed > read_ms => Naming::DisplayedName,
        (naming, _) => naming,
    }
}

/// Join the calling thread to COM's multi-threaded apartment the first time it
/// reads a window through UI Automation. Both threads that read here, the poll's
/// and the watch consumer's, are plain workers owning no window.
fn join_apartment() {
    thread_local!(static JOINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    JOINED.with(|joined| {
        if !joined.replace(true) {
            super::wt_tabs::init_apartment();
        }
    });
}

/// Whether `hwnd` is the window in the foreground.
fn front_of(hwnd: isize, foreground: isize) -> Front {
    if hwnd == foreground {
        Front::Yes
    } else {
        Front::No
    }
}

/// Since when `hwnd` has held the foreground, from a record of the last window
/// that took it and when. A record of another window says nothing about this
/// one. Shared with the agwinterm adapter, whose foreground hook keeps the same
/// record for its own windows, written only from events it was delivered.
pub(super) fn front_since(held: Option<(isize, i64)>, hwnd: isize) -> Since {
    match held {
        Some((held_hwnd, since)) if held_hwnd == hwnd => Since::At(since),
        _ => Since::Unrecorded,
    }
}

/// Whether this dashboard is writing the titles every observation is named by.
fn titles_enabled(app: &AppHandle) -> bool {
    app.try_state::<crate::config::ConfigState>().is_some_and(|c| c.config.lock().unwrap().terminal_titles)
}

/// Diff each title change against what that window last showed, and push a
/// departure when one row was left for another, with the facts read at the
/// event's instant for the verdict to judge.
fn consume(app: &AppHandle, foreground: &Foreground, selections: &Selections, rx: &std::sync::mpsc::Receiver<Raw>, sink: &Sender<Observation>) {
    // Per window, the caption on its tab in front, how it names its session and
    // when that was read, on the caption change that brought the tab forward: the
    // departure from it is judged by the naming read while it could still be
    // read, as long as nothing has been written for its row since.
    let mut last: HashMap<isize, (String, Naming, i64)> = HashMap::new();
    while let Ok(raw) = rx.recv() {
        let ev = match raw {
            Raw::Hooked { front } => {
                *foreground.lock().unwrap() = ForegroundRecord::hooked(front.as_ref().map(|ev| (ev.hwnd, ev.at_ms)));
                if let Some(ev) = &front {
                    caption_seen(app, ev);
                }
                continue;
            }
            Raw::Event(ev) => ev,
        };
        caption_seen(app, &ev);
        foreground.lock().unwrap().delivered(ev.event, ev.hwnd, ev.at_ms);
        if ev.event == EVENT_SYSTEM_FOREGROUND {
            continue;
        }
        let titles = titles_enabled(app);
        // Read while this caption's tab is the one in front, which is the only
        // time it can be.
        let naming = naming_where(titles, ev.hwnd, &ev.title);
        // A window seen here for the first time is not a departure, for the same
        // reason the poll's first sighting is not: nothing was left.
        // Recorded before the feature gate, so turning titling back on resumes
        // against what the window is showing now rather than departing a row off
        // a title from before it was turned off.
        let previous = last.insert(ev.hwnd, (ev.title.clone(), naming, ev.at_ms));
        selections.lock().unwrap().note(ev.hwnd, &ev.title, same_tab, ev.at_ms);
        // The switch predates the event, so crediting the event's own instant
        // would stamp it late. `now_ms` is unreachable here and only satisfies
        // the signature.
        let stamp = super::departure_stamp(previous.as_ref().map(|(t, _, _)| t.as_str()), &ev.title, crate::terminal_title::same_row, Some(ev.at_ms - SWITCH_LATENCY_MS), ev.at_ms);
        let outcome = match (stamp, &previous) {
            _ if !titles => "titles_disabled",
            (Some(at_ms), Some((title, naming, read_ms))) => {
                let naming = naming_at_departure(*naming, *read_ms, crate::terminal_title::last_change_named(app, title));
                if sink.send(observed(Some(title), naming, at_ms, ObservationKind::Departed(switch_at(&ev, *foreground.lock().unwrap())))).is_err() {
                    return; // the consumer is gone; so is the app
                }
                "switched"
            }
            (_, None) => "first_sight",
            (None, Some(_)) => "same_row",
        };
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "watch", outcome, hwnd = ev.hwnd, title = ev.title, naming = ?naming, "active tab title changed");
    }
}

/// What a caption seen on either event kind is worth beyond attention.
fn caption_seen(app: &AppHandle, ev: &RawEvent) {
    // Free of charge, and only here: this caption *is* the active tab's
    // rendered name, so comparing it against what we wrote catches a tab that
    // has stopped following its row. Both event kinds carry one, and a
    // foreground change is worth judging too — it is how a tab that was
    // already stuck comes into view.
    // The window the caption came from, so a lookalike caption in a second
    // window cannot accuse a row this one does not render.
    crate::terminal_title::observe_caption(app, &ev.title, ev.at_ms, Some(&surface_key(ev.hwnd)));
    // A caption change is also the moment to ask whether the tab now in front
    // is showing its own session's title. This is the trigger a write cannot
    // supply: a pinned tab's caption never moves, so switching *to* it
    // produces the only event that says "look at this one now".
    //
    // Gated on the same flag the departure path reads: with titles off
    // nothing is written, so every reading would resolve to no row — and this
    // trigger, unlike the write one, fires on every caption event, so leaving
    // it open meant a full cross-process read of every terminal window per
    // tab switch for a disabled feature.
    if titles_enabled(app) {
        crate::terminals::stale_check::request();
    }
}

impl ForegroundRecord {
    /// The record right after the hooks were installed: the window in front then,
    /// if the foreground hook is among them, not yet trusted.
    fn hooked(front: Option<(isize, i64)>) -> Self {
        Self { held: front, delivered: false }
    }

    /// An event the hooks delivered, which proves they deliver; a foreground one
    /// also says which window took the foreground and when.
    fn delivered(&mut self, event: u32, hwnd: isize, at_ms: i64) {
        self.delivered = true;
        if event == EVENT_SYSTEM_FOREGROUND {
            self.held = Some((hwnd, at_ms));
        }
    }
}

/// The switch a title change reports, with the facts read at its instant: the
/// foreground and the input clock as the hook callback read them, and since when
/// the window has held the foreground as the foreground events recorded it.
fn switch_at(ev: &RawEvent, record: ForegroundRecord) -> Switch {
    let last_input = ev.idle_ms.map_or(LastInput::Unknown, |idle| LastInput::At(ev.at_ms - idle as i64));
    Switch::Placed { latest_ms: ev.at_ms, front: front_of(ev.hwnd, ev.foreground), front_since: record.since(ev.hwnd), last_input }
}

/// Own the hooks and the message loop they need.
///
/// An out-of-context hook is delivered by posting to the registering thread's
/// message queue, so this thread must pump one for the life of the process. The
/// timer is what lets it wake on a silent desktop to notice Windows Terminal
/// starting, or restarting under a new pid.
///
/// Without these hooks this terminal credits nothing: the poll's departures are
/// unplaced, which the verdict refuses, and its input needs the foreground record
/// only the foreground hook keeps. So each way the hooks can be lost is said at
/// warn, and the record is reset so nothing is judged against a stale one.
fn pump() {
    let mut hooks: Vec<isize> = Vec::new();
    let mut hooked: Option<u32> = None;
    unsafe {
        SetTimer(0, 0, PID_RECHECK_MS, 0);
        let mut msg: Msg = std::mem::zeroed();
        loop {
            let pid = terminal_pid();
            if pid != hooked {
                for hook in hooks.drain(..) {
                    UnhookWinEvent(hook);
                }
                hooked = pid;
                let mut foreground_hooked = false;
                if let Some(pid) = pid {
                    // Two narrow ranges rather than one wide one: everything
                    // between these two events would arrive as noise to be
                    // filtered here instead of by the OS.
                    for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_NAMECHANGE] {
                        let hook = SetWinEventHook(event, event, 0, on_event, pid, 0, WINEVENT_OUTOFCONTEXT);
                        if hook == 0 {
                            let lost = if event == EVENT_SYSTEM_FOREGROUND { "when a terminal window came forward is not recorded, so Windows Terminal attention credits neither departures nor input" } else { "tab switches are not seen, so Windows Terminal attention credits no departure, only input" };
                            tracing::warn!(terminal = NAME, pid, event, "could not hook the terminal: {lost}");
                            continue;
                        }
                        foreground_hooked |= event == EVENT_SYSTEM_FOREGROUND;
                        hooks.push(hook);
                    }
                    // An elevated terminal accepts the hook and delivers nothing
                    // to a process at lower integrity, so a handle here is not
                    // yet evidence the watch works; the foreground record stays
                    // untrusted until an event arrives.
                    tracing::info!(terminal = NAME, pid, hooks = hooks.len(), "watching the terminal's active tab");
                }
                // A terminal window already in front has held it since at least
                // now, and no event says so until focus moves. Posted through the
                // hook's own channel, so the consumer records it in order with the
                // events, and only with the foreground hook in place, the one
                // thing that would end it.
                let front = GetForegroundWindow();
                let seed = (foreground_hooked && is_terminal_window(front)).then(|| raw_event(EVENT_SYSTEM_FOREGROUND, front));
                if let Some(tx) = EVENTS.get() {
                    let _ = tx.send(Raw::Hooked { front: seed });
                }
            }
            let got = GetMessageW(&mut msg, 0, 0, 0);
            if got <= 0 {
                // 0 is WM_QUIT and -1 an error; neither is expected on a thread
                // that owns no window, so both end the watch and must say so.
                // The record is reset, since nothing will renew it again.
                if let Some(tx) = EVENTS.get() {
                    let _ = tx.send(Raw::Hooked { front: None });
                }
                tracing::warn!(terminal = NAME, got, "the terminal watch message loop ended: Windows Terminal attention credits neither departures nor input from now on");
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Read the three facts that are only true at this instant, and get out.
unsafe extern "system" fn on_event(_hook: isize, event: u32, hwnd: isize, id_object: i32, id_child: i32, _thread: u32, _time: u32) {
    // A top-level window's own caption, not one of the controls inside it.
    if id_object != OBJID_WINDOW || id_child != CHILDID_SELF {
        return;
    }
    let Some(tx) = EVENTS.get() else { return };
    if !is_terminal_window(hwnd) {
        return;
    }
    let _ = tx.send(Raw::Event(raw_event(event, hwnd)));
}

/// One event about `hwnd`, with the facts that are only true at this instant.
fn raw_event(event: u32, hwnd: isize) -> RawEvent {
    RawEvent {
        event,
        hwnd,
        title: window_text(hwnd),
        // SAFETY: no arguments; answers a handle or zero.
        foreground: unsafe { GetForegroundWindow() },
        idle_ms: crate::idle::idle_ms(),
        at_ms: crate::commands::now_ms(),
    }
}

/// Every visible terminal window and the title of the tab it is showing.
fn terminal_windows() -> Vec<(isize, String)> {
    let mut out: Vec<(isize, String)> = Vec::new();
    unsafe { EnumWindows(collect, &mut out as *mut Vec<(isize, String)> as isize) };
    out
}

unsafe extern "system" fn collect(hwnd: isize, lparam: isize) -> i32 {
    if is_terminal_window(hwnd) {
        if let Some(out) = (lparam as *mut Vec<(isize, String)>).as_mut() {
            out.push((hwnd, window_text(hwnd)));
        }
    }
    1 // keep enumerating
}

/// The process hosting the terminal, or `None` when it is not running.
///
/// One process hosts every terminal window, so this is a single pid however many
/// windows are open — and it is why a window is keyed by its handle everywhere
/// else in this module.
fn terminal_pid() -> Option<u32> {
    let (hwnd, _) = terminal_windows().into_iter().next()?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

/// Whether `hwnd` is a terminal window a user can actually see.
///
/// The **visibility half is not decoration**: Windows Terminal's monarch is an
/// invisible window of this same class, so a class check alone accepts a window
/// no `front_readings` will ever enumerate. [`WindowsAdapter::attached_surface`]
/// shares this rather than keeping a laxer copy — attributing a session's console
/// to the monarch would produce a surface key nothing can match, and since an
/// absent attribution is the permissive answer, that is the one direction it can
/// silently kill both stale-tab oracles at once.
fn is_terminal_window(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(hwnd) != 0 && window_string(|buf, len| GetClassNameW(hwnd, buf, len)) == TERMINAL_CLASS }
}

fn window_text(hwnd: isize) -> String {
    unsafe { window_string(|buf, len| GetWindowTextW(hwnd, buf, len)) }
}

/// The shared half of the two `…W` readers: a stack buffer, the returned length,
/// and lossy decoding. A title longer than the buffer is truncated by the OS,
/// which reads as a title we did not write and so names no row — the safe
/// direction.
pub(super) unsafe fn window_string(read: impl Fn(*mut u16, i32) -> i32) -> String {
    let mut buf = [0u16; 512];
    let len = read(buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;

type WinEventProc = unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32);

/// Only the fields the loop passes back to Windows; `#[repr(C)]` supplies the
/// x64 padding after `message`. Shared with the agwinterm adapter's foreground
/// hook, which pumps a queue of its own and never reads a field either.
#[repr(C)]
pub(super) struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

// Declared by hand to avoid a `windows`/`windows-sys` dep, same as
// `auto_resize::nchittest` and `terminal_title`'s console block.
#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
    fn SetWinEventHook(min: u32, max: u32, hmod: isize, cb: WinEventProc, pid: u32, thread: u32, flags: u32) -> isize;
    fn UnhookWinEvent(hook: isize) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn SetTimer(hwnd: isize, id: usize, elapse: u32, cb: usize) -> usize;
    fn GetAncestor(hwnd: isize, flags: u32) -> isize;
}

// The odd one out: `GetConsoleWindow` is kernel32's, not user32's, despite
// answering with an HWND. `terminal_title` declares it too, for the attach
// dance; a second declaration of the same import is free, and sharing one
// would tie each module's `#[cfg]` tree to the other's.
#[link(name = "kernel32")]
extern "system" {
    fn GetConsoleWindow() -> isize;
}

/// `GA_ROOTOWNER` — walk the owner chain to its root, which for a pseudo-console
/// window is the terminal window hosting it.
const GA_ROOTOWNER: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    const WT: isize = 0xA0A2C;
    const OTHER: isize = 0xBEEF;

    use super::super::{person_verdict, NamedBy, Refusal};

    fn name_change(foreground: isize, idle_ms: Option<u64>) -> RawEvent {
        RawEvent { event: EVENT_OBJECT_NAMECHANGE, hwnd: WT, title: "🟢 b".into(), foreground, idle_ms, at_ms: 10_000 }
    }

    /// A foreground record the hooks have proven, holding `hwnd` since `since`.
    fn held(hwnd: isize, since: i64) -> ForegroundRecord {
        ForegroundRecord { held: Some((hwnd, since)), delivered: true }
    }

    /// A departure from the tab titled `🟢 a`, named by the pane's own title.
    fn departed(switch: Switch) -> Observation {
        observed(Some("🟢 a"), Naming::OwnTitle, 9_750, ObservationKind::Departed(switch))
    }

    #[test]
    fn a_title_change_carries_the_foreground_and_input_clock_of_its_instant() {
        // Measured: a real tab switch arrives with input under 50 ms old.
        assert_eq!(switch_at(&name_change(WT, Some(16)), held(WT, 1_000)), Switch::Placed { latest_ms: 10_000, front: Front::Yes, front_since: Since::At(1_000), last_input: LastInput::At(9_984) });
    }

    #[test]
    fn a_human_switch_passes_the_verdict_with_nothing_said_about_the_pane() {
        // What keeps this terminal working: its observations are named by the
        // console title this dashboard wrote, so the unknown occupant is not
        // asked.
        assert_eq!(person_verdict(&departed(switch_at(&name_change(WT, Some(16)), held(WT, 1_000))), NamedBy::Title), Ok(()));
    }

    #[test]
    fn a_scripted_switch_is_reported_as_it_happened_and_refused() {
        // `wt.exe focus-tab` from a script, measured driving a background window,
        // and one driving the window in front while nobody types.
        let behind = departed(switch_at(&name_change(OTHER, Some(16)), held(OTHER, 1_000)));
        assert_eq!(person_verdict(&behind, NamedBy::Title), Err(Refusal::NotInFront));
        let idle = departed(switch_at(&name_change(WT, Some(6_407)), held(WT, 1_000)));
        assert_eq!(person_verdict(&idle, NamedBy::Title), Err(Refusal::NoRecentInput));
        let unknown = departed(switch_at(&name_change(WT, None), held(WT, 1_000)));
        assert_eq!(person_verdict(&unknown, NamedBy::Title), Err(Refusal::InputUnknown));
    }

    #[test]
    fn a_click_on_a_tab_of_a_window_behind_another_is_refused() {
        // One click brought the window forward and switched its tab; the tab it
        // left had been behind another window.
        assert_eq!(person_verdict(&departed(switch_at(&name_change(WT, Some(16)), held(WT, 9_950))), NamedBy::Title), Err(Refusal::ActivatedBySwitch));
    }

    #[test]
    fn a_foreground_record_speaks_only_for_its_own_window() {
        assert_eq!(front_since(Some((WT, 1_000)), WT), Since::At(1_000));
        assert_eq!(front_since(Some((WT, 1_000)), OTHER), Since::Unrecorded);
        assert_eq!(front_since(None, WT), Since::Unrecorded);
    }

    #[test]
    fn the_window_in_front_when_the_hooks_went_in_is_trusted_only_once_they_deliver() {
        // An elevated terminal accepts the hooks and delivers nothing, so a record
        // seeded at startup would never be renewed or ended: typing in a browser
        // later would pass for typing into the tab on screen.
        let mut record = ForegroundRecord::hooked(Some((WT, 1_000)));
        assert_eq!(record.since(WT), Since::Unrecorded);
        record.delivered(EVENT_OBJECT_NAMECHANGE, WT, 5_000);
        assert_eq!(record.since(WT), Since::At(1_000), "a delivered event proves the hooks reach this process");
        record.delivered(EVENT_SYSTEM_FOREGROUND, OTHER, 6_000);
        assert_eq!((record.since(WT), record.since(OTHER)), (Since::Unrecorded, Since::At(6_000)));
        assert_eq!(ForegroundRecord::hooked(None).since(WT), Since::Unrecorded, "hooks reinstalled, or lost, start from nothing");
    }

    #[test]
    fn input_reaches_only_the_window_that_has_held_the_foreground_since_before_it() {
        let input = |front, since, at_ms| {
            let facts = InputFacts { front, front_since: since, selection: Selection::Live, selected_since: Since::At(0), cover: Cover::Clear };
            person_verdict(&observed(Some("🟢 a"), Naming::OwnTitle, at_ms, ObservationKind::Input(facts)), NamedBy::Title)
        };
        assert_eq!(input(front_of(WT, WT), held(WT, 1_000).since(WT), 9_600), Ok(()));
        assert_eq!(input(front_of(WT, WT), held(WT, 9_900).since(WT), 9_600), Err(Refusal::InputBeforeFront), "typing before the terminal came forward was typing elsewhere");
        assert_eq!(input(front_of(WT, WT), held(WT, 9_500).since(WT), 9_600), Err(Refusal::ForegroundInput), "the click that brought the window forward, released after it came");
        assert_eq!(input(front_of(WT, OTHER), held(WT, 1_000).since(WT), 9_600), Err(Refusal::NotInFront), "a stale record must not credit typing in another program");
        assert_eq!(input(front_of(OTHER, OTHER), held(WT, 1_000).since(OTHER), 9_600), Err(Refusal::ForegroundUnrecorded), "one terminal window's record is not another's");
    }

    #[test]
    fn a_selections_start_is_kept_only_through_an_unchanged_caption() {
        // `bga` and `bga assistant` share a label, so a clock comparing by row
        // would keep the first tab's start for the second; under exact equality
        // each caption starts its own selection.
        let mut clock = SelectionClock::default();
        assert_eq!(clock.note(WT, "🟢 bga", same_tab, 0), 0);
        assert_eq!(clock.note(WT, "🟢 bga assistant", same_tab, 20_000), 20_000, "another tab starts its own selection");
        assert_eq!(clock.note(WT, "🟢 bga assistant", same_tab, 25_000), 20_000);
        // Our own rewrite of the tab in front restarts it, which refuses only
        // input made within the release margin of that write.
        assert_eq!(clock.note(WT, "🔵 bga assistant", same_tab, 30_000), 30_000);
    }

    #[test]
    fn a_caption_is_the_sessions_own_title_only_when_a_pane_behind_it_carries_it() {
        let panes = |titles: &[&str]| titles.iter().map(|t| Some(t.to_string())).collect::<Vec<_>>();
        assert_eq!(naming_from("🟢 dash", Some(&panes(&["🟢 dash"]))), Naming::OwnTitle);
        assert_eq!(naming_from("🟢 dash", Some(&panes(&["Windows PowerShell", "🟢 dash"]))), Naming::OwnTitle, "a split whose agent pane the tab follows");
        // A tab renamed by hand at `🟢 dash` and left after its agent exited: the
        // pane behind it is a plain shell.
        assert_eq!(naming_from("🟢 dash", Some(&panes(&["Windows PowerShell"]))), Naming::DisplayedName);
        assert_eq!(naming_from("🟢 dash", Some(&[None])), Naming::DisplayedName, "a pane whose title could not be read");
        assert_eq!(naming_from("🟢 dash", None), Naming::DisplayedName, "a window that could not be read");
    }

    #[test]
    fn a_caption_read_before_its_rows_title_moved_is_only_a_displayed_name() {
        // Tab A was read as `🟢 dash` and then pinned; its agent exited, the row
        // went and came back in tab B, whose first write moved the row's title
        // after tab A's read without moving tab A's caption.
        assert_eq!(naming_at_departure(Naming::OwnTitle, 1_000, Some(5_000)), Naming::DisplayedName);
        let o = observed(Some("🟢 dash"), naming_at_departure(Naming::OwnTitle, 1_000, Some(5_000)), 9_750, ObservationKind::Departed(switch_at(&name_change(WT, Some(16)), held(WT, 1_000))));
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::OccupantUnknown(OCCUPANT_UNREPORTED)));
        // A write before the read is one the caption followed.
        assert_eq!(naming_at_departure(Naming::OwnTitle, 5_000, Some(1_000)), Naming::OwnTitle);
        assert_eq!(naming_at_departure(Naming::OwnTitle, 5_000, None), Naming::OwnTitle, "a caption no row's title names");
        assert_eq!(naming_at_departure(Naming::DisplayedName, 5_000, Some(1_000)), Naming::DisplayedName);
    }

    #[test]
    fn a_departure_from_a_tab_showing_a_name_its_pane_does_not_carry_is_refused() {
        // Nothing says what runs in a Windows Terminal pane, so a displayed name
        // that is not the pane's own title leaves the verdict nothing to check.
        let o = observed(Some("🟢 dash"), Naming::DisplayedName, 9_750, ObservationKind::Departed(switch_at(&name_change(WT, Some(16)), held(WT, 1_000))));
        assert_eq!(person_verdict(&o, NamedBy::Title), Err(Refusal::OccupantUnknown(OCCUPANT_UNREPORTED)));
    }

    /// The departure rules this adapter inherits, exercised through its own
    /// comparator — the poll and the watch both run exactly this.
    fn stamp(previous: Option<&str>, current: &str, last: Option<i64>, now: i64) -> Option<i64> {
        super::super::departure_stamp(previous, current, crate::terminal_title::same_row, last, now)
    }

    #[test]
    fn our_own_rewrite_of_the_tab_in_front_is_not_a_switch() {
        // Both measured live on this machine: a glyph moving when the user
        // prompts the session already on screen, and the context suffix ticking
        // while it works. Calling either a switch marks the row read on our own
        // write.
        assert_eq!(stamp(Some("🟢 bga-assistant"), "🔵 bga-assistant", Some(1_000), 6_000), None);
        assert_eq!(stamp(Some("🟢 what-is-next [76%]"), "🟢 what-is-next [77%]", Some(1_000), 6_000), None);
        assert_eq!(stamp(Some("✋ dash [62%]"), "✋ dash [62%] ⚠", Some(1_000), 6_000), None, "a drift badge appearing");
    }

    #[test]
    fn leaving_one_row_for_another_departs_the_one_left_behind() {
        assert_eq!(stamp(Some("🟢 transcripts"), "🟢 what-is-next [76%]", Some(1_000), 6_000), Some(1_000));
    }

    #[test]
    fn leaving_a_tracked_tab_for_a_plain_shell_still_departs_it() {
        // The commonest switch on this machine, and the one a design that
        // discarded unrecognized titles would silently lose.
        assert_eq!(stamp(Some("🟢 transcripts"), "powershell", Some(1_000), 6_000), Some(1_000));
    }

    #[test]
    fn a_window_showing_nothing_of_ours_departs_nothing_we_can_name() {
        // It still produces an observation — the previous title is what it is —
        // and `attention::resolve_row` finds no row for it and logs `no_target`.
        assert_eq!(stamp(Some("powershell"), "🟢 transcripts", Some(1_000), 6_000), Some(1_000));
    }

    #[test]
    fn the_first_sight_of_a_window_is_not_a_departure() {
        // At startup every window's tab is new to us and there is no earlier
        // session to have left; calling it one would mark whatever is on screen
        // as read.
        assert_eq!(stamp(None, "🟢 transcripts", Some(1_000), 6_000), None);
    }

    #[test]
    fn a_departure_is_credited_to_the_previous_reading() {
        // Known only to within an interval, so crediting `now` would mark a row
        // that finished *during* it as read by a departure that predated it.
        assert_eq!(stamp(Some("🟢 a"), "🟢 b", Some(1_000), 6_000), Some(1_000));
        assert_eq!(stamp(Some("🟢 a"), "🟢 b", None, 6_000), Some(6_000), "and to now when there is no previous reading");
    }
}
