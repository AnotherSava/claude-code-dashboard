//! The agterm adapter: reading which session the user is in, and when they left.
//!
//! Two readings per poll, per open window, from one `tree --json` call each:
//!
//! - **The departure.** The selected session going from S to T means the user
//!   *left* S, and that is what marks S read — leaving is the moment you are done
//!   with what was on screen. Arriving at T marks nothing: you have not read T
//!   yet, and marking on arrival meant merely passing through a finished tab
//!   marked it, which is what this replaced.
//! - **The input instant.** `idleMs` is per window and is a *duration*, so
//!   `poll_start - idleMs` converts it to the absolute instant input last
//!   happened. That conversion is why the sampling rate does not affect
//!   correctness here: a reading taken thirty seconds late reports the same
//!   instant as one taken immediately, and only the pill's latency suffers.
//!
//! Both are per *window* because both facts are: `idleMs` covers the projected
//! window, and each window has its own selection. A bare `tree` projects only the
//! frontmost one, so a second window's rows would never resolve and would sit
//! marked unread forever — indistinguishable from "he has not read them", which is
//! how a marker like this stops being believed.
//!
//! **The poll is the safety net, not the primary source.** Sampling a *level*
//! (which tab is selected) to detect an *edge* (the user left it) misses any visit
//! that begins and ends between two polls — observed in production 2026-09-02 —
//! and no interval closes that, since the interval is pure discovery lag rather
//! than a chosen delay. [`AgtermAdapter::watch`] is the answer: agterm writes
//! `selectedSessionID` on a ~0.3s debounce, so a short visit writes *twice* there
//! where a poll sees it zero times. What survives is the debounce itself — an
//! enter-and-leave inside 300ms still coalesces to one write. See
//! `.claude/memory/attention_visit_detection_design.md`.

use super::{Observation, ObservationKind, TerminalAdapter, TerminalSession};
use std::collections::HashMap;

/// How long the open-window list is trusted before re-enumerating. Windows are
/// opened and closed by hand, so this is slow-moving state; the TTL is what keeps
/// multi-window support from doubling the single-window subprocess cost.
const WINDOW_LIST_TTL_MS: i64 = 60_000;

#[derive(Default)]
pub struct AgtermAdapter {
    /// The open window ids, and when they were enumerated.
    windows: Option<(i64, Vec<String>)>,
    /// Which session was selected in each window at the previous poll — the whole
    /// departure signal. A different one now means the user left that session
    /// somewhere in between.
    last_selected: HashMap<String, String>,
    /// The previous poll's clock, used as the conservative departure instant.
    last_poll_at: Option<i64>,
}

impl TerminalAdapter for AgtermAdapter {
    fn name(&self) -> &'static str {
        "agterm"
    }

    /// Watch agterm's per-window snapshots and report a departure the moment the
    /// selection changes — ~300ms after the switch, agterm's own save debounce,
    /// rather than at the next tick.
    ///
    /// Three things this has to get right, all of them from how the file is
    /// written rather than what it contains:
    ///
    /// - **Watch the directory, not the files.** The save is atomic — write to a
    ///   temp file, then replace — so the inode is swapped on every write and a
    ///   file-level watch goes deaf after the first one, which looks exactly like
    ///   the user having stopped switching tabs.
    /// - **A write is not a departure.** The same save is triggered by renames,
    ///   moves, reordering, sidebar width and recency. Only a changed
    ///   `selectedSessionID` counts, so every write is a *re-read and diff*.
    /// - **A new window's first read is not a departure**, for the same reason the
    ///   poll's first sighting is not: there is no earlier session to have left.
    fn watch(&self, sink: std::sync::mpsc::Sender<Observation>) {
        let Some(dir) = windows_dir() else {
            tracing::warn!(terminal = "agterm", "no HOME; the selection watcher cannot start");
            return;
        };
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                // Create, modify and rename are all "this window was rewritten",
                // because the atomic save produces them interchangeably.
                if matches!(event.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_)) {
                    let _ = tx.send(());
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(terminal = "agterm", error = %e, "selection watcher create failed; falling back to the poll");
                    return;
                }
            };
            if let Err(e) = notify::Watcher::watch(&mut watcher, &dir, notify::RecursiveMode::NonRecursive) {
                tracing::warn!(terminal = "agterm", dir = %dir.display(), error = %e, "selection watch failed; falling back to the poll");
                return;
            }
            tracing::info!(terminal = "agterm", "watching the selection snapshot");
            let mut selected: HashMap<std::path::PathBuf, String> = HashMap::new();
            while rx.recv().is_ok() {
                // agterm's save debounce is ~300ms; a short burst of events for one
                // switch collapses here into a single re-read of every window.
                while rx.recv_timeout(std::time::Duration::from_millis(120)).is_ok() {}
                for observation in reread(&dir, &mut selected) {
                    if sink.send(observation).is_err() {
                        return; // the consumer is gone; so is the app
                    }
                }
            }
        });
    }

    /// Every tab agterm currently holds, across every workspace of every open
    /// window.
    ///
    /// Per-window, for the same reason [`poll`](Self::poll) is: a bare `tree`
    /// projects only the **frontmost** window, and asking again returns that same
    /// window, so a session in a background one would not merely be late — it
    /// would be permanently invisible, and the caller's "is anything missing?"
    /// gate would stay true forever, paying a subprocess on every tick to
    /// rediscover the same nothing. `poll` learned this already; the stake here is
    /// higher, since a missed session has no row at all rather than a stale read
    /// marker.
    ///
    /// A window it cannot read is skipped rather than failing the whole answer: a
    /// window closing between the list and the read is ordinary, and the sessions
    /// in the windows that *did* answer are still worth returning. `None` is
    /// reserved for having been unable to ask agterm at all, which is the only
    /// case the caller must not read as "there are no tabs".
    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        let list = crate::agterm::agtermctl(&["window", "list", "--json"])?;
        let mut out = Vec::new();
        for window in crate::agterm::open_window_ids(&list) {
            let Some(tree) = crate::agterm::agtermctl(&["tree", "--json", "--window", &window]) else {
                tracing::debug!(terminal = "agterm", window = %window, "no tree for this window; its sessions are not restorable this pass");
                continue;
            };
            out.extend(crate::agterm::session_nodes(&tree).map(session_from_node));
        }
        Some(out)
    }

    fn poll(&mut self, now_ms: i64) -> Vec<Observation> {
        let mut out = Vec::new();
        for window in self.windows_to_poll(now_ms) {
            let Some(tree) = crate::agterm::agtermctl(&["tree", "--json", "--window", &window]) else {
                tracing::debug!(decision = "attention_poll", terminal = "agterm", outcome = "no_answer", window = %window, "agterm gave no tree");
                continue;
            };
            let Some(reading) = parse_reading(&tree) else {
                tracing::debug!(decision = "attention_poll", terminal = "agterm", outcome = "no_selection", window = %window, "tree names no selected session");
                continue;
            };
            let previous = self.last_selected.insert(window.clone(), reading.session_id.clone());
            let reading = with_departed(reading, &tree, previous.as_deref());
            let outcome = match departure_stamp(previous.as_deref(), &reading.session_id, self.last_poll_at, now_ms) {
                Some(at_ms) => {
                    // The row the user *left*, which is the one they were reading —
                    // not the one they arrived at.
                    out.push(Observation { session: reading.departed.clone(), at_ms, kind: ObservationKind::Departed });
                    "departed"
                }
                None => "same_session",
            };
            // `idleMs` still describes the session now on screen: the user is in
            // it, so input in this window is input to it.
            if let Some(idle_ms) = reading.idle_ms {
                out.push(Observation { session: reading.selected.clone(), at_ms: now_ms - idle_ms as i64, kind: ObservationKind::Input });
            }
            // Logged on every poll, including the ones observing nothing: a sensor
            // whose success and whose total failure are both silent cannot be told
            // apart from one that never ran (`feedback_not_run_is_not_pass`).
            tracing::debug!(
                decision = "attention_poll",
                terminal = "agterm",
                outcome,
                window = %window,
                selected = ?reading.selected.title,
                has_idle = reading.idle_ms.is_some(),
                "agterm poll"
            );
        }
        self.last_poll_at = Some(now_ms);
        out
    }
}

/// agterm's persisted per-window state, one file per window.
///
/// Private, undocumented state with no compatibility promise, which is why
/// [`SNAPSHOT_VERSION`] is checked and an unexpected shape degrades to the poll
/// rather than being guessed at.
fn windows_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Application Support/agterm/windows"))
}

/// agterm's own save debounce, from `AppStore.scheduleSave`. A selection change
/// reaches disk this long after it happened, so it is subtracted from the
/// observation time to err early — and it is also the residual blind window, since
/// an enter-and-leave inside it coalesces into one write carrying only the final
/// state.
const SAVE_DEBOUNCE_MS: i64 = 300;

/// The `version` this adapter knows how to read. A different one means the
/// layout may have moved under us; the watcher stands down and the poll carries
/// on, logged, rather than producing confident wrong departures.
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
pub fn parse_snapshot(value: &serde_json::Value) -> Option<Snapshot> {
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(SNAPSHOT_VERSION) {
        return None;
    }
    let mut cwds = HashMap::new();
    for ws in value.get("workspaces").and_then(serde_json::Value::as_array).into_iter().flatten() {
        for s in ws.get("sessions").and_then(serde_json::Value::as_array).into_iter().flatten() {
            if let (Some(id), Some(cwd)) = (s.get("id").and_then(serde_json::Value::as_str), s.get("cwd").and_then(serde_json::Value::as_str)) {
                cwds.insert(id.to_string(), cwd.to_string());
            }
        }
    }
    Some(Snapshot { selected_session_id: value.get("selectedSessionID").and_then(serde_json::Value::as_str).map(str::to_string), cwds })
}

impl AgtermAdapter {
    /// The open windows to read this pass, re-enumerated at most every
    /// [`WINDOW_LIST_TTL_MS`].
    ///
    /// An empty answer — agterm closed, or the call failed — falls back to
    /// `active`, agterm's own name for the frontmost window and what a
    /// `--window`-less call resolves to, so the degraded path needs no second
    /// branch.
    fn windows_to_poll(&mut self, now_ms: i64) -> Vec<String> {
        if let Some((at, ids)) = self.windows.as_ref() {
            if now_ms - at < WINDOW_LIST_TTL_MS {
                return ids.clone();
            }
        }
        let mut ids = crate::agterm::agtermctl(&["window", "list", "--json"]).map(|l| crate::agterm::open_window_ids(&l)).unwrap_or_default();
        if ids.is_empty() {
            ids.push("active".to_string());
        }
        self.windows = Some((now_ms, ids.clone()));
        ids
    }
}

/// Re-read every window snapshot and report whoever was departed since last time.
///
/// The whole diff lives here so the watcher thread stays a thin loop: read, diff,
/// name, emit. `selected` is the previous `selectedSessionID` per window file,
/// and a window absent from it is being seen for the first time — which is not a
/// departure, since nothing was left.
#[cfg(target_os = "macos")]
fn reread(dir: &std::path::Path, selected: &mut HashMap<std::path::PathBuf, String>) -> Vec<Observation> {
    // The switch happened *before* the write we are reacting to — agterm coalesces
    // for `SAVE_DEBOUNCE_MS` first — so crediting the read time would stamp the
    // departure late, marking content that arrived after the user had already
    // gone. Backing it off by the debounce errs early, which only leaves a row
    // showing.
    let at_ms = crate::commands::now_ms() - SAVE_DEBOUNCE_MS;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(snapshot) = parse_snapshot(&value) else {
            tracing::warn!(terminal = "agterm", file = %path.display(), "unrecognized snapshot version; the poll stays the only source for this window");
            continue;
        };
        let Some(now_selected) = snapshot.selected_session_id.clone() else { continue };
        let previous = selected.insert(path.clone(), now_selected.clone());
        let Some(previous) = previous.filter(|p| *p != now_selected) else { continue };
        // The file has no title, and `resolve_row` wants one — a `cd`-ed session
        // reports a cwd deriving another row's id. One `tree` call on this edge
        // buys it; the snapshot's own cwd is the fallback when that fails.
        let session = departed_session(&previous, snapshot.cwds.get(&previous).map(String::as_str));
        tracing::debug!(terminal = "agterm", decision = "attention_poll", outcome = "departed", source = "watch", title = ?session.title, "selection snapshot changed");
        out.push(Observation { session, at_ms, kind: ObservationKind::Departed });
    }
    out
}

/// Read a session node's two portable handles.
///
/// The one place that knows how an agterm session node maps onto a
/// [`TerminalSession`] — three copies of this had grown, and a terminal that ever
/// exposes a third handle would have been given it by only some of them.
fn session_from_node(node: &serde_json::Value) -> TerminalSession {
    TerminalSession {
        cwd: node.get("cwd").and_then(serde_json::Value::as_str).map(str::to_string),
        title: node.get("title").and_then(serde_json::Value::as_str).map(str::to_string),
    }
}

/// Name the departed session, preferring the live title over the snapshot's cwd.
#[cfg(target_os = "macos")]
fn departed_session(id: &str, snapshot_cwd: Option<&str>) -> TerminalSession {
    let from_tree = crate::agterm::agtermctl(&["tree", "--json"])
        .and_then(|tree| crate::agterm::session_node(&tree, id).cloned())
        .map(|n| session_from_node(&n));
    from_tree.unwrap_or_else(|| TerminalSession { cwd: snapshot_cwd.map(str::to_string), title: None })
}

/// One window's reading: who is selected, who was left behind, and the idle clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// agterm's own id for the selected session. The key for detecting a *change*
    /// of selection, and deliberately not the dashboard's row id: several agterm
    /// sessions can share one row, and switching between two tabs of the same
    /// project is a real departure from the first.
    pub session_id: String,
    /// The session now on screen.
    pub selected: TerminalSession,
    /// The session left behind, when the previous one is still in the tree.
    /// Falls back to the selected one only when it is not, which cannot mark
    /// anything wrongly — [`departure_stamp`] gates on the id having changed.
    pub departed: TerminalSession,
    /// Milliseconds since the last user input in this window. `None` before any
    /// activity, which agterm reports by omitting the field — the absence of
    /// evidence rather than evidence of a visit.
    pub idle_ms: Option<u64>,
}

/// Read one window's `tree --json` answer.
///
/// Pure and fixture-pinned: the shape this depends on is agterm's private state,
/// so a schema change should break a test here rather than silently make every
/// session look unattended.
pub fn parse_reading(tree: &serde_json::Value) -> Option<Reading> {
    let idle_ms = tree.get("result")?.get("tree")?.get("idleMs").and_then(serde_json::Value::as_u64);
    let active = crate::agterm::session_nodes(tree).find(|s| s.get("active").and_then(serde_json::Value::as_bool) == Some(true))?;
    let selected = session_from_node(active);
    Some(Reading {
        session_id: active.get("id").and_then(serde_json::Value::as_str)?.to_string(),
        departed: selected.clone(),
        selected,
        idle_ms,
    })
}

/// Fill in which session was left behind, now that the caller knows its id.
///
/// Separate from [`parse_reading`] because the previous selection is the
/// adapter's state, not the tree's — the tree only says who is selected *now*.
pub fn with_departed(mut reading: Reading, tree: &serde_json::Value, previous_id: Option<&str>) -> Reading {
    if let Some(prev) = previous_id {
        if let Some(node) = crate::agterm::session_node(tree, prev) {
            reading.departed = session_from_node(node);
        }
    }
    reading
}

/// The instant to credit a departure to, or `None` when nobody left.
///
/// A departure is a *change* of selection: a different session is on screen now,
/// so the user left the previous one somewhere in between. That is what marks a
/// row read — leaving is the moment you are done with what was on screen, and
/// reading itself produces no keystroke to observe.
///
/// The stamp is the **previous** poll rather than now, because the change is known
/// only to within a poll interval and crediting it to `now` would mark a row that
/// finished *during* that interval as read by a departure that came before it. A
/// first observation is deliberately not a departure — at startup every window's
/// selection is new to us, and there is no earlier session to have left.
pub fn departure_stamp(previous: Option<&str>, now_selected: &str, last_poll_at: Option<i64>, now_ms: i64) -> Option<i64> {
    let switched = previous.is_some_and(|p| p != now_selected);
    switched.then(|| last_poll_at.unwrap_or(now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(active_id: &str, active_title: &str, idle_ms: Option<u64>) -> serde_json::Value {
        let mut t = serde_json::json!({"workspaces": [
            {"name": "apps", "sessions": [
                {"id": "S1", "cwd": "/p/one", "title": "🟢 one", "active": active_id == "S1"},
                {"id": "S2", "cwd": "/p/two", "title": active_title, "active": active_id == "S2"},
            ]},
        ]});
        if let Some(ms) = idle_ms {
            t["idleMs"] = serde_json::json!(ms);
        }
        serde_json::json!({"ok": true, "result": {"tree": t}})
    }

    /// Trimmed from the live file, so a layout change breaks a test here rather
    /// than silently stopping every departure.
    fn snapshot(version: u64, selected: &str) -> serde_json::Value {
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
        // hide finished work; standing down leaves the poll as the only source.
        assert!(parse_snapshot(&snapshot(2, "S2")).is_none());
        assert!(parse_snapshot(&serde_json::json!({"selectedSessionID": "S2"})).is_none(), "no version at all");
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

    #[test]
    fn a_reading_names_the_selected_session_and_the_idle_clock() {
        let r = parse_reading(&tree("S2", "🔵 two", Some(4_000))).expect("reading");
        assert_eq!(r.session_id, "S2");
        assert_eq!(r.selected.title.as_deref(), Some("🔵 two"));
        assert_eq!(r.selected.cwd.as_deref(), Some("/p/two"));
        assert_eq!(r.idle_ms, Some(4_000));
    }

    #[test]
    fn a_window_with_no_input_yet_reports_no_idle_clock() {
        // agterm omits `idleMs` before any activity. The absence of evidence, not
        // evidence that nobody is there.
        assert_eq!(parse_reading(&tree("S2", "🔵 two", None)).expect("reading").idle_ms, None);
    }

    #[test]
    fn an_unrecognized_tree_yields_nothing_rather_than_a_guess() {
        assert!(parse_reading(&serde_json::json!({"ok": true})).is_none());
        let none_active = serde_json::json!({"ok": true, "result": {"tree": {"idleMs": 10, "workspaces": [
            {"name": "apps", "sessions": [{"id": "S1", "cwd": "/p/one", "active": false}]},
        ]}}});
        assert!(parse_reading(&none_active).is_none(), "agterm open but the user is nowhere");
    }

    #[test]
    fn the_departed_session_is_the_one_left_behind_not_the_one_arrived_at() {
        // The point of the whole model: leaving S1 for S2 marks *S1* read.
        let t = tree("S2", "🔵 two", Some(0));
        let r = with_departed(parse_reading(&t).expect("reading"), &t, Some("S1"));
        assert_eq!(r.departed.title.as_deref(), Some("🟢 one"));
        assert_eq!(r.selected.title.as_deref(), Some("🔵 two"), "and the arrival is still known, for its idle clock");
    }

    #[test]
    fn a_departed_session_gone_from_the_tree_falls_back_to_the_selected_one() {
        // Closing a tab removes it; nothing is mis-marked, because a stamp is only
        // produced when the id changed and the fallback then names a live session.
        let t = tree("S2", "🔵 two", Some(0));
        let r = with_departed(parse_reading(&t).expect("reading"), &t, Some("GONE"));
        assert_eq!(r.departed.title.as_deref(), Some("🔵 two"));
    }

    #[test]
    fn leaving_a_tab_is_credited_to_the_previous_poll() {
        // Known only to within an interval, so crediting `now` would mark a row
        // that finished *during* it as read by a departure that predated it.
        assert_eq!(departure_stamp(Some("S1"), "S2", Some(1_000), 6_000), Some(1_000));
    }

    #[test]
    fn staying_on_one_tab_is_not_a_departure() {
        assert_eq!(departure_stamp(Some("S2"), "S2", Some(1_000), 6_000), None);
    }

    #[test]
    fn the_first_sight_of_a_window_is_not_a_departure() {
        // At startup every selection is new to us and there is no earlier session
        // to have left; calling it one would mark whatever is on screen as read.
        assert_eq!(departure_stamp(None, "S2", Some(1_000), 6_000), None);
    }
}
