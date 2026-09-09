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
//! **The watch is the primary source and the poll is the safety net**, and here
//! that ordering is starker than on macOS. Sampling a level to catch an edge
//! misses any visit that begins and ends between two polls, and measured on this
//! machine real visits run 1.7 and 2.2 seconds against a 30-second tick.
//! [`WindowsAdapter::watch`] takes the edge directly from
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
//!   event — while script-driven ones did not, so [`human_switch`] gates the watch
//!   on both. The poll cannot make that judgment (it learns of the switch some
//!   time after the fact and the foreground has moved on) and does not try, which
//!   is the honest difference between knowing an instant and knowing an interval.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Manager};

use super::{FrontReading, Observation, ObservationKind, TerminalAdapter, TerminalSession};

/// The slug the decision log carries. Deliberately the platform and not
/// `"windows_terminal"`: attention really is Windows Terminal's, but
/// [`WindowsAdapter::sessions`] reads console objects and so answers for a VS
/// Code terminal or a bare conhost too, and a slug naming WT would be false on
/// every `restore_scan` line for one of those.
const NAME: &str = "windows";

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

/// How recent the last desktop input must be, at the instant a title change
/// arrives, for the switch to be attributable to a person. Measured, a human tab
/// switch lands at under 50 ms — the switch is itself a click or a keystroke — so
/// this is loose by two orders of magnitude and still refuses the script-driven
/// case it exists for.
const SWITCH_INPUT_WINDOW_MS: u64 = 2_000;

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
static EVENTS: OnceLock<Sender<RawEvent>> = OnceLock::new();

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
type Foreground = Arc<Mutex<Option<(isize, i64)>>>;

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
}

impl WindowsAdapter {
    pub fn new(app: AppHandle) -> Self {
        Self { app, last_title: HashMap::new(), last_poll_at: None, foreground: Arc::new(Mutex::new(None)) }
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
        let held = *self.foreground.lock().unwrap();
        let idle_ms = crate::idle::idle_ms();
        for (hwnd, title) in terminal_windows() {
            let previous = self.last_title.insert(hwnd, title.clone());
            let outcome = match super::departure_stamp(previous.as_deref(), &title, crate::terminal_title::same_row, self.last_poll_at, now_ms) {
                Some(at_ms) => {
                    // The row the user *left*, which is the one they were
                    // reading — not the one they arrived at.
                    out.push(Observation { session: named(previous.as_deref()), at_ms, kind: ObservationKind::Departed });
                    "departed"
                }
                None if previous.is_none() => "first_sight",
                None => "same_row",
            };
            let typed = input_stamp(hwnd, foreground_now, held, idle_ms, now_ms);
            if let Some(at_ms) = typed {
                out.push(Observation { session: named(Some(&title)), at_ms, kind: ObservationKind::Input });
            }
            // Logged on every window every pass, including the ones observing
            // nothing: a sensor whose success and whose total failure are both
            // silent cannot be told apart from one that never ran.
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome, hwnd, title, typed = typed.is_some(), "windows terminal poll");
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
        std::thread::spawn(move || consume(&app, &foreground, &rx, &sink));
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

/// Whether this dashboard is writing the titles every observation is named by.
fn titles_enabled(app: &AppHandle) -> bool {
    app.try_state::<crate::config::ConfigState>().is_some_and(|c| c.config.lock().unwrap().terminal_titles)
}

/// The instant to credit input in `hwnd` to, or `None` when this pass cannot say
/// the user typed into that window.
///
/// `GetLastInputInfo` is desktop-wide and on its own says nothing about any
/// window — it reads "not idle" while the user is deep in a game. Composing it
/// with the foreground is still not enough on its own: input a moment ago plus
/// this window in front *now* does not establish the input went here.
///
/// So the test is that the input instant falls inside a stretch this window has
/// **already** held the foreground for, which is an interval the
/// `EVENT_SYSTEM_FOREGROUND` hook records the start of, and that it still holds
/// it at read time — that second half is what stops a stale `since` from
/// crediting an hour of typing in a browser to whatever tab is on screen.
///
/// The stamp is absolute (`now - idle`), so the tick rate changes how fast the
/// pill catches up and never whether it is right. Unknown idle or no recorded
/// foreground is refused rather than guessed: nothing here infers attention from
/// an absence.
fn input_stamp(hwnd: isize, foreground_now: isize, held: Option<(isize, i64)>, idle_ms: Option<u64>, now_ms: i64) -> Option<i64> {
    let (held_hwnd, since) = held?;
    if held_hwnd != hwnd || foreground_now != hwnd {
        return None;
    }
    let at_ms = now_ms - idle_ms? as i64;
    (at_ms >= since).then_some(at_ms)
}

/// Whether a title change is attributable to a person at this keyboard.
///
/// A tab switch is itself a click or a keystroke, so a human one always arrives
/// with the window in front and the input clock at nearly zero. A script driving
/// `wt.exe focus-tab` produces the same event with neither, and crediting it
/// would mark a row read that nobody looked at.
fn human_switch(event_hwnd: isize, foreground: isize, idle_ms: Option<u64>) -> bool {
    event_hwnd == foreground && idle_ms.is_some_and(|ms| ms <= SWITCH_INPUT_WINDOW_MS)
}

/// Diff each title change against what that window last showed, and push a
/// departure when a person left one row for another.
fn consume(app: &AppHandle, foreground: &Foreground, rx: &std::sync::mpsc::Receiver<RawEvent>, sink: &Sender<Observation>) {
    let mut last: HashMap<isize, String> = HashMap::new();
    while let Ok(ev) = rx.recv() {
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
        // Gated on the same flag the departure path below reads: with titles off
        // nothing is written, so every reading would resolve to no row — and this
        // trigger, unlike the write one, fires on every caption event, so leaving
        // it open meant a full cross-process read of every terminal window per
        // tab switch for a disabled feature.
        if titles_enabled(app) {
            crate::terminals::stale_check::request();
        }
        if ev.event == EVENT_SYSTEM_FOREGROUND {
            *foreground.lock().unwrap() = Some((ev.hwnd, ev.at_ms));
            continue;
        }
        // A window seen here for the first time is not a departure, for the same
        // reason the poll's first sighting is not: nothing was left.
        // Recorded before the feature gate, so turning titling back on resumes
        // against what the window is showing now rather than departing a row off
        // a title from before it was turned off.
        let previous = last.insert(ev.hwnd, ev.title.clone());
        // The switch predates the event, so crediting the event's own instant
        // would stamp it late. `now_ms` is unreachable here and only satisfies
        // the signature.
        let stamp = super::departure_stamp(previous.as_deref(), &ev.title, crate::terminal_title::same_row, Some(ev.at_ms - SWITCH_LATENCY_MS), ev.at_ms);
        let outcome = match stamp {
            _ if !titles_enabled(app) => "titles_disabled",
            Some(_) if !human_switch(ev.hwnd, ev.foreground, ev.idle_ms) => "not_human",
            Some(at_ms) => {
                if sink.send(Observation { session: named(previous.as_deref()), at_ms, kind: ObservationKind::Departed }).is_err() {
                    return; // the consumer is gone; so is the app
                }
                "departed"
            }
            None if previous.is_none() => "first_sight",
            None => "same_row",
        };
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "watch", outcome, hwnd = ev.hwnd, title = ev.title, "active tab title changed");
    }
}

/// Own the hooks and the message loop they need.
///
/// An out-of-context hook is delivered by posting to the registering thread's
/// message queue, so this thread must pump one for the life of the process. The
/// timer is what lets it wake on a silent desktop to notice Windows Terminal
/// starting, or restarting under a new pid.
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
                if let Some(pid) = pid {
                    // Two narrow ranges rather than one wide one: everything
                    // between these two events would arrive as noise to be
                    // filtered here instead of by the OS.
                    for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_NAMECHANGE] {
                        let hook = SetWinEventHook(event, event, 0, on_event, pid, 0, WINEVENT_OUTOFCONTEXT);
                        if hook == 0 {
                            tracing::warn!(terminal = NAME, pid, event, "could not hook the terminal; the poll stays the only source");
                            continue;
                        }
                        hooks.push(hook);
                    }
                    // An elevated terminal accepts the hook and delivers nothing
                    // to a process at lower integrity, so a handle here is not
                    // yet evidence the watch works.
                    tracing::info!(terminal = NAME, pid, hooks = hooks.len(), "watching the terminal's active tab");
                }
            }
            let got = GetMessageW(&mut msg, 0, 0, 0);
            if got <= 0 {
                // 0 is WM_QUIT and -1 an error; neither is expected on a thread
                // that owns no window, so both end the watch and must say so
                // rather than leaving the poll silently alone.
                tracing::warn!(terminal = NAME, got, "the terminal watch message loop ended; the poll stays the only source");
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
    let _ = tx.send(RawEvent {
        event,
        hwnd,
        title: window_text(hwnd),
        foreground: GetForegroundWindow(),
        idle_ms: crate::idle::idle_ms(),
        at_ms: crate::commands::now_ms(),
    });
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
unsafe fn window_string(read: impl Fn(*mut u16, i32) -> i32) -> String {
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
/// x64 padding after `message`.
#[repr(C)]
struct Msg {
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

    #[test]
    fn input_is_credited_at_the_instant_it_happened_not_at_the_poll() {
        // Absolute, so a reading taken late reports the same instant as one taken
        // immediately and only the pill's latency suffers.
        assert_eq!(input_stamp(WT, WT, Some((WT, 1_000)), Some(400), 10_000), Some(9_600));
    }

    #[test]
    fn input_before_the_terminal_took_the_foreground_is_refused() {
        // The whole point of the gate: typing at 9_600 while the terminal only
        // came forward at 9_900 was typing somewhere else.
        assert_eq!(input_stamp(WT, WT, Some((WT, 9_900)), Some(400), 10_000), None);
    }

    #[test]
    fn input_is_refused_once_the_foreground_has_moved_on() {
        // A stale `since` must not credit an hour of typing in a browser to
        // whatever tab happens to be on screen.
        assert_eq!(input_stamp(WT, OTHER, Some((WT, 1_000)), Some(400), 10_000), None);
    }

    #[test]
    fn input_in_one_terminal_window_is_not_credited_to_another() {
        assert_eq!(input_stamp(OTHER, OTHER, Some((WT, 1_000)), Some(400), 10_000), None);
    }

    #[test]
    fn an_unknown_idle_clock_or_foreground_yields_no_input_observation() {
        // Nothing infers attention from an absence.
        assert_eq!(input_stamp(WT, WT, Some((WT, 1_000)), None, 10_000), None);
        assert_eq!(input_stamp(WT, WT, None, Some(400), 10_000), None);
    }

    #[test]
    fn a_switch_with_the_window_in_front_and_input_just_now_is_a_person() {
        // Measured: a real tab switch arrives at under 50 ms, being itself a
        // click or a keystroke.
        assert!(human_switch(WT, WT, Some(16)));
        assert!(human_switch(WT, WT, Some(SWITCH_INPUT_WINDOW_MS)));
    }

    #[test]
    fn a_switch_in_a_window_nobody_is_looking_at_is_not_a_person() {
        // `wt.exe focus-tab` from a script, measured driving a background window.
        assert!(!human_switch(WT, OTHER, Some(16)));
    }

    #[test]
    fn a_switch_with_no_recent_input_is_not_a_person() {
        assert!(!human_switch(WT, WT, Some(6_407)));
        assert!(!human_switch(WT, WT, None));
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
