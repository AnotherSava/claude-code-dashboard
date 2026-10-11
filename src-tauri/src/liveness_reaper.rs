//! Drops row members whose Claude process has exited without a `SessionEnd`
//! reaching the dashboard, and removes a row once its last member is gone.
//!
//! `SessionEnd` fires cleanly on `/clear`, but not reliably on `exit` / Ctrl-D /
//! terminal close (see [`crate::liveness`] for the why). When it doesn't fire,
//! the row is stranded — e.g. the user cancels a prompt with Esc and types
//! `exit` before the watcher settles it, leaving the row wedged in `Working`
//! with its console already gone.
//!
//! This task is the backstop. Each tick it takes one process enumeration and
//! checks every pid-keyed member of every row ([`Members::pid_members`]) — not
//! every row, so a member inside a `/clear` gap, whose row is momentarily gone,
//! is still judged. A member is dropped only after its pid reads dead for
//! [`DEAD_STREAK_TO_REAP`] consecutive ticks, counted per (row, pid) pair, so a
//! still-alive (merely slow) session can never be dropped and one dead process
//! never borrows another's count. The drop and what follows it go through
//! [`crate::commands::apply_departure`], the path a `SessionEnd` takes, so a
//! reaped row restores cleanly (with history) on its next start.
//!
//! Only the judged pids are dropped, under the row's lock, so a session that
//! joined the row since the last read keeps it: that is what replaces comparing
//! the row's `updated` against the one observed.
//!
//! `reap_exited_sessions` gates only removing a row whose last member died. The
//! dead member is dropped either way, so a dead process never stays in charge
//! of a row.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::{now_ms, LeaveVia};
use crate::config::ConfigState;
use crate::liveness::{is_live_claude, process_images};
use crate::membership::Members;

/// Poll cadence. Reaping a vanished session is a backstop, not latency-critical,
/// so a slower 2s tick is fine.
const POLL: Duration = Duration::from_secs(2);

/// Consecutive confirmed-dead reads required before a member is dropped. Rides
/// out any one-off enumeration oddity. At [`POLL`] this is a ~6s reap latency,
/// fine for a backstop.
const DEAD_STREAK_TO_REAP: u32 = 3;

/// Consecutive dead reads per (row, pid).
#[derive(Default)]
struct ReapTracker {
    streaks: HashMap<(String, u32), u32>,
}

impl ReapTracker {
    /// Record a confirmed-dead read; returns the running streak length.
    fn record_dead(&mut self, row: &str, pid: u32) -> u32 {
        let count = self.streaks.entry((row.to_string(), pid)).or_insert(0);
        *count += 1;
        *count
    }

    fn reset(&mut self, row: &str, pid: u32) {
        self.streaks.remove(&(row.to_string(), pid));
    }

    /// Forget the pairs that are no longer members, so a pid that leaves and is
    /// later reused starts from nothing.
    fn retain(&mut self, members: &HashSet<(String, u32)>) {
        self.streaks.retain(|pair, _| members.contains(pair));
    }
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tracker = ReapTracker::default();
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("liveness reaper started");

        loop {
            ticker.tick().await;

            let Some(members) = app.try_state::<Members>() else { continue };

            // One enumeration per tick. If it fails we can't prove anything is
            // dead — skip the whole tick (streaks untouched, never a false reap).
            let Some(images) = process_images() else { continue };

            let pairs: HashSet<(String, u32)> = members.pid_members().into_iter().collect();
            tracker.retain(&pairs);

            let mut judged: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            for (row, pid) in &pairs {
                // Present: alive iff still claude. Absent from a full snapshot: gone.
                let alive = is_live_claude(&images, *pid);
                if alive {
                    tracker.reset(row, *pid);
                } else if tracker.record_dead(row, *pid) >= DEAD_STREAK_TO_REAP {
                    judged.entry(row.clone()).or_default().push(*pid);
                }
            }

            for (row, pids) in judged {
                for pid in &pids {
                    tracker.reset(&row, *pid);
                }
                // Off the async workers: the row lock can be held by a hook
                // event in the middle of a 12MB history write.
                let app = app.clone();
                if let Err(e) = tauri::async_runtime::spawn_blocking(move || reap_row(&app, &row, &pids)).await {
                    tracing::error!(error = %e, "reaping a row failed");
                }
            }
        }
    });
}

/// Drop `pids` from `row` and act on what that leaves.
fn reap_row(app: &AppHandle, row: &str, pids: &[u32]) {
    let remove_row = app.try_state::<ConfigState>().is_none_or(|c| c.snapshot().reap_exited_sessions);
    let row_lock = app.try_state::<crate::commands::RowLocks>().map(|locks| locks.row(row));
    let _row_guard = row_lock.as_deref().map(crate::commands::RowLocks::hold);
    let Some(members) = app.try_state::<Members>() else { return };
    let now = now_ms();
    if let Some(left) = members.drop_dead(row, pids, now) {
        crate::commands::apply_departure(app, row, &left, LeaveVia::Reaped { pids, remove_row }, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streak_counts_consecutive_dead_reads() {
        let mut t = ReapTracker::default();
        assert_eq!(t.record_dead("a", 100), 1);
        assert_eq!(t.record_dead("a", 100), 2);
        assert_eq!(t.record_dead("a", 100), 3);
    }

    #[test]
    fn each_member_of_a_row_keeps_its_own_streak() {
        // The incident's shape: a probe that died beside a live main must be
        // counted alone, and the main's live reads must not reset the probe.
        let mut t = ReapTracker::default();
        t.record_dead("dash", 51_016);
        t.reset("dash", 34_390);
        assert_eq!(t.record_dead("dash", 51_016), 2);
        assert_eq!(t.record_dead("dash", 34_390), 1);
        assert_eq!(t.record_dead("web", 51_016), 1, "the same pid in another row is another pair");
    }

    #[test]
    fn reset_clears_the_streak() {
        let mut t = ReapTracker::default();
        t.record_dead("a", 100);
        t.reset("a", 100);
        assert_eq!(t.record_dead("a", 100), 1);
    }

    #[test]
    fn a_pair_that_is_no_longer_a_member_is_forgotten() {
        let mut t = ReapTracker::default();
        t.record_dead("a", 100);
        t.record_dead("a", 200);
        t.retain(&HashSet::from([("a".to_string(), 200)]));
        assert_eq!(t.record_dead("a", 100), 1, "a reused pid starts from nothing");
        assert_eq!(t.record_dead("a", 200), 2);
    }
}
