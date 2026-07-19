//! Settles a local `Waiting` row to `Done` once it has sat in WAIT for the
//! configured grace window without any status change.
//!
//! `Waiting` ("looks done but isn't") is entered at `Stop` from the hook's
//! `background_tasks` and is normally left when the background work finishes and
//! the follow-up turn's `Stop` settles the row. But a background *shell* task
//! the user *kills* — e.g. a dev server via the Claude UI (down-arrow → x) —
//! ends silently: it fires no hook and writes nothing to the JSONL transcript,
//! so nothing ever clears the row and it sits in WAIT until the user's next
//! prompt. This tick is the backstop that settles it to `Done`.
//!
//! The backstop only covers WAITs the adapter flagged `waiting_backstop_armed`
//! (a `shell` task is in flight — see `adapters::claude::classify_stop`). A
//! *subagent-only* WAIT is deliberately excluded: a background subagent always
//! resolves with a completion turn, so time-settling one that's still running
//! would falsely mark a live subagent `Done`. So there is no time window that
//! can end a subagent-held WAIT — only its own completion turn does.
//!
//! For an armed WAIT it's pure time-in-state. `state_entered_at` is reset by any
//! real status change (a completion's follow-up turn promotes WAIT→Working, a
//! new prompt starts a fresh turn), and a legitimate finite shell task (tests,
//! CI-watches, builds) self-resolves that way well within the window — a history
//! scan showed them capping around ~9 min. So only a stuck, killed-shell WAIT
//! ever ages past the window; legitimate work is promoted off `Waiting` long
//! before it's reached.
//!
//! The settle is guarded by the row's `updated` (via
//! [`crate::state::AppState::settle_stale_waiting`]) so an event landing between
//! the snapshot and the mutation aborts it. Local rows only — remote WAITs
//! settle on their origin device. Cross-platform; gated by
//! `config.waiting_settle_ms` (`None`/`0` disables). A false settle (a rare
//! background task that legitimately outlasts the window) self-corrects: its
//! completion's follow-up turn promotes the row Done→Working→Done.

use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::ConfigState;
use crate::state::{AppState, Status};

/// Poll cadence. A 10-minute-scale grace window needs no fine granularity, so a
/// coarse tick keeps the cost negligible (one small in-memory scan).
const POLL: Duration = Duration::from_secs(30);

/// True when a row has sat in `Waiting` (unchanged) for at least `window_ms`
/// *and* the WAIT is backstop-armed — held by a silently-killable `shell`
/// background task. A subagent-only WAIT (`backstop_armed == false`) is never
/// time-settled: a background subagent always resolves with a completion turn,
/// so settling one that's still running would falsely mark it Done.
/// Pure for testing; `window_ms == 0` disables (matches a `None` config).
fn should_settle(status: Status, backstop_armed: bool, state_entered_at: i64, now_ms: i64, window_ms: u64) -> bool {
    window_ms > 0
        && status == Status::Waiting
        && backstop_armed
        && now_ms.saturating_sub(state_entered_at) >= window_ms as i64
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("waiting-settle backstop started");

        loop {
            ticker.tick().await;

            let Some(cfg) = app.try_state::<ConfigState>() else { continue };
            let window = cfg.snapshot().waiting_settle_ms.unwrap_or(0);
            if window == 0 {
                continue;
            }
            let Some(app_state) = app.try_state::<AppState>() else { continue };

            let now = now_ms();
            for s in app_state.snapshot() {
                if !should_settle(s.status, s.waiting_backstop_armed, s.state_entered_at, now, window) {
                    continue;
                }
                // `s.updated` guards against a follow-up turn / new prompt that
                // landed since this snapshot — the settle aborts rather than
                // clobbering a row that just moved on.
                if app_state.settle_stale_waiting(&s.id, s.updated, now) {
                    tracing::debug!(
                        chat_id = %s.id,
                        decision = "settle_waiting",
                        waited_ms = now.saturating_sub(s.state_entered_at),
                        window_ms = window,
                        reason = "background work ended without a signal (e.g. a user-killed dev server); settled Waiting -> Done after the grace window",
                        "decision"
                    );
                    crate::commands::emit_sessions_updated(&app);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: u64 = 600_000; // 10 min

    #[test]
    fn armed_waiting_past_window_settles() {
        assert!(should_settle(Status::Waiting, true, 0, WINDOW as i64, WINDOW));
        assert!(should_settle(Status::Waiting, true, 0, WINDOW as i64 + 1, WINDOW));
    }

    #[test]
    fn waiting_within_window_holds() {
        assert!(!should_settle(Status::Waiting, true, 0, WINDOW as i64 - 1, WINDOW));
    }

    #[test]
    fn subagent_only_waiting_never_settles() {
        // A disarmed (subagent-only) WAIT is left to its completion turn — no
        // time window ever ends it, however long it's aged.
        assert!(!should_settle(Status::Waiting, false, 0, WINDOW as i64 * 10, WINDOW));
    }

    #[test]
    fn non_waiting_never_settles() {
        // Only Waiting is time-settled; every other state is hook-authoritative.
        for st in [Status::Idle, Status::Working, Status::Blocked, Status::Done, Status::Error] {
            assert!(!should_settle(st, true, 0, WINDOW as i64 * 10, WINDOW));
        }
    }

    #[test]
    fn zero_window_disables() {
        // `None`/`0` config → the feature is off even for a long-stale armed WAIT.
        assert!(!should_settle(Status::Waiting, true, 0, i64::MAX / 2, 0));
    }
}
