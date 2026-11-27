//! Sessions another agent is running, whose status this dashboard stops
//! announcing to the user for as long as that agent holds them.
//!
//! A coordinating session (the user's shepherd) briefs other sessions through
//! `SendMessage` and reads their results itself. While it does, every turn the
//! briefed session ends would otherwise ping the user about work they did not
//! ask for and are not the one reading. `POST /api/session/control` marks a
//! project's row as controlled and releases it again; while marked, the row's
//! tab shows `terminal_title::CONTROLLED_GLYPH` in place of a status glyph, the
//! widget's pill reads HERD, and `notifications::reconcile` neither fires nor
//! keeps a per-state ping for it. The row's own state machine runs on untouched
//! underneath, so a release shows the status the row really holds.
//!
//! **A mark must not outlive the run that set it.** It is tied to the
//! controller's process, resolved once from its Claude Code session id when the
//! mark is set: the session id changes when the controller is `/clear`ed, and
//! the process does not. The liveness reaper's tick drops a mark whose
//! controller has read dead on `liveness_reaper::DEAD_STREAK_TO_REAP`
//! consecutive readings of its process table ([`lapse_marks`]), and one not
//! renewed for [`EXPIRY_MS`], which covers a controller that is alive and
//! stopped without releasing. Each control call renews. A mark also ends when its row is
//! removed (`commands::remove_session`), since a `/clear` in the marked session
//! is how the controller finishes with it and a row's last session leaving
//! means nothing is left to control.
//!
//! The marks are persisted, because the dashboard is redeployed while runs are
//! in progress and the controller does not re-mark on a restart. Keyed by row
//! id, written before the row exists where the controller opened the session
//! itself, so a session started for a run is quiet from its first turn.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::json_store;
use crate::liveness::DeadStreaks;
use crate::liveness_reaper::DEAD_STREAK_TO_REAP;
use crate::state::AgentSession;

pub const FILE_NAME: &str = "controlled_sessions.json";

/// How long a mark lasts without a renewal. The controller renews at least
/// hourly while it runs anything, and one of its sessions can sit controlled
/// for a whole run of a few hours while queued, so this is several runs long.
pub const EXPIRY_MS: i64 = 12 * 60 * 60 * 1000;

/// One controlled row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// The project folder the controller named, kept so a later run can
    /// release a stale mark by the same address.
    pub dir: String,
    /// The controller's Claude Code session id when it last marked the row.
    pub controller_session: String,
    /// The controller's process, which the mark's life is tied to.
    pub controller_pid: u32,
    pub since_ms: i64,
    pub renewed_ms: i64,
}

impl Mark {
    pub fn expires_ms(&self) -> i64 {
        self.renewed_ms.saturating_add(EXPIRY_MS)
    }
}

/// Why a mark ended without a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lapse {
    ControllerGone,
    Expired,
}

impl Lapse {
    pub fn slug(self) -> &'static str {
        match self {
            Lapse::ControllerGone => "controller_gone",
            Lapse::Expired => "expired",
        }
    }
}

/// Whether a mark lapses on this tick: expiry first, since it needs no process
/// table, then a controller dead for long enough. `dead_reads` already counts
/// this tick's reading.
pub fn lapse(mark: &Mark, dead_reads: u32, now_ms: i64) -> Option<Lapse> {
    if now_ms >= mark.expires_ms() {
        Some(Lapse::Expired)
    } else if dead_reads >= DEAD_STREAK_TO_REAP {
        Some(Lapse::ControllerGone)
    } else {
        None
    }
}

/// What setting a mark did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marked {
    Controlled,
    Renewed,
}

impl Marked {
    pub fn slug(self) -> &'static str {
        match self {
            Marked::Controlled => "controlled",
            Marked::Renewed => "renewed",
        }
    }
}

#[derive(Default)]
struct Held {
    marks: BTreeMap<String, Mark>,
    /// Consecutive dead readings per controller pid. In memory only: a restart
    /// starts every count again, which can delay a lapse by up to
    /// `DEAD_STREAK_TO_REAP - 1` ticks.
    dead: DeadStreaks<u32>,
}

/// Mark a row in `marks`: a repeat renews and keeps `since_ms`, taking the
/// newer controller identity, since the same run re-briefs a session and a
/// controller's session id moves on a `/clear`.
fn set_mark(marks: &mut BTreeMap<String, Mark>, row: &str, mut mark: Mark) -> Marked {
    match marks.get(row) {
        Some(prev) => {
            mark.since_ms = prev.since_ms;
            marks.insert(row.to_string(), mark);
            Marked::Renewed
        }
        None => {
            marks.insert(row.to_string(), mark);
            Marked::Controlled
        }
    }
}

/// Update the dead counts from one process-table reading and take out every
/// mark that lapses. `images` is that reading, `None` where the table could not
/// be read, which leaves every count where it was while expiry still applies.
fn take_lapsed(held: &mut Held, images: Option<&HashMap<u32, String>>, now_ms: i64) -> Vec<(String, Mark, Lapse)> {
    // One reading per controller, however many sessions it holds.
    let pids: BTreeSet<u32> = held.marks.values().map(|m| m.controller_pid).collect();
    held.dead.retain(|pid| pids.contains(pid));
    if let Some(images) = images {
        for pid in pids {
            if crate::liveness::is_live_claude(images, pid) {
                held.dead.reset(&pid);
            } else {
                held.dead.record_dead(pid);
            }
        }
    }
    let lapsed: Vec<(String, Lapse)> = held
        .marks
        .iter()
        .filter_map(|(row, m)| lapse(m, held.dead.streak(&m.controller_pid), now_ms).map(|l| (row.clone(), l)))
        .collect();
    lapsed.into_iter().filter_map(|(row, l)| held.marks.remove(&row).map(|m| (row, m, l))).collect()
}

pub struct ControlMarks {
    path: PathBuf,
    held: Mutex<Held>,
}

impl ControlMarks {
    pub fn new(path: PathBuf) -> Self {
        let marks = json_store::load(&path, "controlled sessions");
        Self { path, held: Mutex::new(Held { marks, dead: DeadStreaks::default() }) }
    }

    fn save(&self, marks: &BTreeMap<String, Mark>) {
        json_store::save(&self.path, marks, "controlled sessions");
    }

    pub fn mark(&self, row: &str, mark: Mark) -> Marked {
        let mut held = self.held.lock().unwrap();
        let outcome = set_mark(&mut held.marks, row, mark);
        self.save(&held.marks);
        outcome
    }

    /// End a row's mark, released or because the row went; `None` where it
    /// had none.
    pub fn end(&self, row: &str) -> Option<Mark> {
        let mut held = self.held.lock().unwrap();
        let mark = held.marks.remove(row)?;
        self.save(&held.marks);
        Some(mark)
    }

    pub fn list(&self) -> Vec<(String, Mark)> {
        self.held.lock().unwrap().marks.iter().map(|(r, m)| (r.clone(), m.clone())).collect()
    }

    /// Stamp `AgentSession::controlled` on the local rows of `sessions`. A
    /// synced row keeps the value its own machine pushed. The one stamping
    /// point the display path, the notifier and the sync push all call, the way
    /// `CustomNamesStore::apply` is for names.
    pub fn apply(&self, sessions: &mut [AgentSession]) {
        let held = self.held.lock().unwrap();
        for s in sessions.iter_mut().filter(|s| s.origin.is_none()) {
            s.controlled = held.marks.contains_key(&s.id);
        }
    }

    fn take_lapsed(&self, images: Option<&HashMap<u32, String>>, now_ms: i64) -> Vec<(String, Mark, Lapse)> {
        let mut held = self.held.lock().unwrap();
        let lapsed = take_lapsed(&mut held, images, now_ms);
        if !lapsed.is_empty() {
            self.save(&held.marks);
        }
        lapsed
    }
}

/// Log a mark ending, on the `session_control` line every change to a mark
/// writes.
pub fn log_ended(row: &str, mark: &Mark, outcome: &str) {
    tracing::info!(
        chat_id = %row,
        decision = "session_control",
        outcome,
        controller_session = %mark.controller_session,
        controller_pid = mark.controller_pid,
        held_ms = crate::commands::now_ms().saturating_sub(mark.since_ms),
        "session control ended"
    );
}

/// Lapse the marks whose controller has gone or that were never renewed, off
/// the process table the liveness reaper read this tick (`None` where it could
/// not be read). Called from the reaper's async loop, so the emit, which waits
/// on the Tauri main thread, runs on a blocking thread. The lapse itself runs
/// inline on that loop: when a mark ends, the rewrite of `FILE_NAME` happens on
/// the async worker with the marks' lock held, so a slow disk stalls that
/// worker and any control call or emit waiting on the lock for as long as the
/// write takes. Nothing it holds the lock across waits on the main thread.
pub fn lapse_marks(app: &AppHandle, images: Option<&HashMap<u32, String>>) {
    let Some(marks) = app.try_state::<ControlMarks>() else { return };
    let lapsed = marks.take_lapsed(images, crate::commands::now_ms());
    if lapsed.is_empty() {
        return;
    }
    for (row, mark, why) in &lapsed {
        log_ended(row, mark, why.slug());
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || crate::commands::emit_sessions_updated(&app));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process table in which only `pids` run Claude Code.
    fn only_alive(pids: &[u32]) -> HashMap<u32, String> {
        pids.iter().map(|&p| (p, "claude.exe".to_string())).collect()
    }

    fn mark(pid: u32, at: i64) -> Mark {
        Mark { dir: "/p/proj".into(), controller_session: "s1".into(), controller_pid: pid, since_ms: at, renewed_ms: at }
    }

    #[test]
    fn a_repeat_renews_and_keeps_when_control_began() {
        let mut marks = BTreeMap::new();
        assert_eq!(set_mark(&mut marks, "proj", mark(7, 100)), Marked::Controlled);
        let mut again = mark(7, 500);
        again.controller_session = "s2".into();
        assert_eq!(set_mark(&mut marks, "proj", again), Marked::Renewed);
        let held = &marks["proj"];
        assert_eq!((held.since_ms, held.renewed_ms, held.controller_session.as_str()), (100, 500, "s2"));
    }

    #[test]
    fn a_mark_expires_from_its_last_renewal() {
        let m = mark(7, 1_000);
        assert_eq!(lapse(&m, 0, 1_000 + EXPIRY_MS - 1), None);
        assert_eq!(lapse(&m, 0, 1_000 + EXPIRY_MS), Some(Lapse::Expired));
    }

    #[test]
    fn one_dead_reading_does_not_drop_a_runs_marks() {
        let mut held = Held::default();
        held.marks.insert("a".into(), mark(7, 0));
        held.marks.insert("b".into(), mark(7, 0));
        held.marks.insert("c".into(), mark(9, 0));
        let dead_7 = only_alive(&[9]);
        for tick in 1..DEAD_STREAK_TO_REAP {
            assert!(take_lapsed(&mut held, Some(&dead_7), i64::from(tick)).is_empty(), "too few dead readings");
        }
        let gone = take_lapsed(&mut held, Some(&dead_7), 100);
        let rows: Vec<&str> = gone.iter().map(|(r, _, l)| {
            assert_eq!(*l, Lapse::ControllerGone);
            r.as_str()
        }).collect();
        assert_eq!(rows, ["a", "b"]);
        assert!(held.marks.contains_key("c"), "another controller's mark stays");
    }

    #[test]
    fn a_live_reading_resets_the_dead_count() {
        let mut held = Held::default();
        held.marks.insert("a".into(), mark(7, 0));
        assert!(take_lapsed(&mut held, Some(&only_alive(&[])), 10).is_empty());
        assert!(take_lapsed(&mut held, Some(&only_alive(&[7])), 20).is_empty());
        assert_eq!(held.dead.streak(&7), 0, "the count started again");
    }

    #[test]
    fn an_unreadable_table_leaves_the_dead_count_alone() {
        let mut held = Held::default();
        held.marks.insert("a".into(), mark(7, 0));
        take_lapsed(&mut held, Some(&only_alive(&[])), 10);
        assert!(take_lapsed(&mut held, None, 20).is_empty());
        assert_eq!(held.dead.streak(&7), 1, "a failed read neither advances nor breaks the streak");
        let expired = take_lapsed(&mut held, None, EXPIRY_MS);
        assert_eq!(expired.iter().map(|(_, _, l)| *l).collect::<Vec<_>>(), [Lapse::Expired], "expiry needs no table");
    }

    #[test]
    fn apply_stamps_local_rows_and_leaves_a_synced_rows_pushed_value() {
        let dir = std::env::temp_dir().join(format!("ccdash-control-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let marks = ControlMarks::new(dir.join(FILE_NAME));
        marks.mark("proj", mark(7, 0));
        use crate::state::{test_row, test_synced_row, Status};
        let local = test_row("proj", Status::Done, 0, None);
        let mut other = test_row("other", Status::Done, 0, None);
        other.controlled = true; // a stamp left from an earlier pass
        let mut synced = test_synced_row("AIR", "proj", 0);
        synced.controlled = true;
        let mut rows = vec![local, other, synced];
        marks.apply(&mut rows);
        assert_eq!(rows.iter().map(|s| s.controlled).collect::<Vec<_>>(), [true, false, true]);
        assert!(marks.end("proj").is_some());
        assert!(ControlMarks::new(dir.join(FILE_NAME)).list().is_empty(), "the end was persisted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
