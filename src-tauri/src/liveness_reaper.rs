//! Removes a local session row whose owning Claude process has exited without a
//! `SessionEnd` reaching the dashboard.
//!
//! `SessionEnd` fires cleanly on `/clear`, but not reliably on `exit` / Ctrl-D /
//! terminal close (see [`crate::liveness`] for the why). When it doesn't fire,
//! the row is stranded — e.g. the user cancels a prompt with Esc and types
//! `exit`, leaving the row wedged in `Working`. `idle_probe` can't recover that
//! one either: it needs to read the terminal screen, but the console is gone.
//!
//! This task is the backstop. Each tick it takes one process enumeration and,
//! for every local session with a hook-reported owning pid ([`AgentPids`]),
//! checks whether that pid is still a live claude. A row is reaped only after
//! the pid reads dead for [`DEAD_STREAK_TO_REAP`] consecutive ticks over an
//! unchanged pid and a quiet `updated` timestamp — so a still-alive (merely
//! slow) session, whose claude process stays alive, can never be reaped, and a
//! same-cwd restart (new live pid, bumped `updated`) restarts the count rather
//! than deleting the fresh row. Removal goes through the shared
//! [`crate::commands::remove_session`] — the exact path `SessionEnd` uses — so a
//! reaped row restores cleanly (with history) on its next start, and it is
//! guarded by the row's `updated` to abort if an event lands mid-reap.
//!
//! Cross-platform (the enumeration in [`crate::liveness::process_images`] has a
//! macOS implementation), unlike `idle_probe`'s Windows-only console read.
//! Reaps rows in any state — matching SessionEnd, which removes the row
//! regardless of what it last showed.

use std::collections::HashMap;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::ConfigState;
use crate::liveness::{is_claude_image, process_images, AgentPids};
use crate::state::AppState;

/// Poll cadence. Reaping a vanished session is a backstop, not latency-critical,
/// so this is slower than `idle_probe`'s 1s.
const POLL: Duration = Duration::from_secs(2);

/// Consecutive confirmed-dead reads — over an unchanged owning pid and a quiet
/// row — required before a row is reaped. Rides out the brief window in which a
/// same-cwd restart re-reports a fresh live pid, plus any one-off enumeration
/// oddity. At [`POLL`] this is a ~6s reap latency, fine for a backstop.
const DEAD_STREAK_TO_REAP: u32 = 3;

#[derive(Clone, Copy)]
struct DeadStreak {
    pid: u32,
    updated: i64,
    count: u32,
}

/// Per-chat_id consecutive-dead bookkeeping. The owning process being dead is
/// the primary signal; the streak (with pid + `updated` reset) is the guard
/// against transient reads and the reap-vs-restart race.
#[derive(Default)]
struct ReapTracker {
    streaks: HashMap<String, DeadStreak>,
}

impl ReapTracker {
    /// Record a confirmed-dead read for `id`; returns the running streak length.
    /// Restarts the count when the owning `pid` changed (a same-cwd restart
    /// overwrote it) or the row's `updated` advanced (a real event landed) —
    /// either means this isn't the same quiet, dead session we were counting.
    fn record_dead(&mut self, id: &str, pid: u32, updated: i64) -> u32 {
        let e = self.streaks.entry(id.to_string()).or_insert(DeadStreak { pid, updated, count: 0 });
        if e.pid != pid || e.updated != updated {
            *e = DeadStreak { pid, updated, count: 0 };
        }
        e.count += 1;
        e.count
    }

    fn reset(&mut self, id: &str) {
        self.streaks.remove(id);
    }

    fn retain<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.streaks.retain(|id, _| keep(id));
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

            let Some(cfg) = app.try_state::<ConfigState>() else { continue };
            if !cfg.snapshot().reap_exited_sessions {
                tracker.streaks.clear();
                continue;
            }
            let Some(app_state) = app.try_state::<AppState>() else { continue };
            let Some(agent_pids) = app.try_state::<AgentPids>() else { continue };

            // One enumeration per tick. If it fails we can't prove anything is
            // dead — skip the whole tick (streaks untouched, never a false reap).
            let Some(images) = process_images() else { continue };

            let sessions = app_state.snapshot(); // local sessions only
            tracker.retain(|id| sessions.iter().any(|s| s.id == id));

            for s in &sessions {
                let Some(pid) = agent_pids.get(&s.id) else {
                    // No owning pid reported (pre-field hook, or a node-based
                    // install the hook couldn't resolve) — can't judge; leave it.
                    tracker.reset(&s.id);
                    continue;
                };
                let alive = match images.get(&pid) {
                    Some(img) => is_claude_image(img), // present: alive iff still claude
                    None => false,                     // absent from a full snapshot: gone
                };
                if alive {
                    tracker.reset(&s.id);
                    continue;
                }
                if tracker.record_dead(&s.id, pid, s.updated) >= DEAD_STREAK_TO_REAP {
                    // `Some(s.updated)` makes remove_session abort if an event
                    // landed since this snapshot — closes the reap-vs-restart race.
                    if crate::commands::remove_session(&app, &s.id, Some(s.updated), now_ms()) {
                        tracing::debug!(
                            chat_id = %s.id,
                            decision = "reap_exited",
                            pid,
                            prior_status = ?s.status,
                            reason = "owning Claude process exited without a SessionEnd (exit / Ctrl-D / terminal close); row removed",
                            "decision"
                        );
                    }
                    tracker.reset(&s.id);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streak_counts_consecutive_dead_reads() {
        let mut t = ReapTracker::default();
        assert_eq!(t.record_dead("a", 100, 5), 1);
        assert_eq!(t.record_dead("a", 100, 5), 2);
        assert_eq!(t.record_dead("a", 100, 5), 3);
    }

    #[test]
    fn pid_change_restarts_streak() {
        // A same-cwd restart reports a new live pid; the dead streak for the old
        // pid must not carry over and reap the freshly-restarted row.
        let mut t = ReapTracker::default();
        t.record_dead("a", 100, 5);
        assert_eq!(t.record_dead("a", 200, 5), 1);
    }

    #[test]
    fn updated_change_restarts_streak() {
        // A new event bumped the row's `updated`, so it isn't the quiet, dead
        // row we were counting — start over.
        let mut t = ReapTracker::default();
        t.record_dead("a", 100, 5);
        assert_eq!(t.record_dead("a", 100, 6), 1);
    }

    #[test]
    fn reset_clears_the_streak() {
        let mut t = ReapTracker::default();
        t.record_dead("a", 100, 5);
        t.reset("a");
        assert_eq!(t.record_dead("a", 100, 5), 1);
    }
}
