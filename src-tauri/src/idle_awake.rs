//! Keeps macOS awake while a local agent is working, so a turn running with the
//! lid open is not killed by the idle-sleep timer.
//!
//! Claude Code holds a `caffeinate -i -t 300` of its own and renews it per turn,
//! and a lapse in that renewal ends the turn with "Your computer went to sleep
//! mid-response". It happened nine times in the month to 2026-09-24 across six
//! projects on this machine, once traceable to the second: a final assistant
//! message stamped 17:04:20.522Z against a `pmset -g log` line reading `Entering
//! Sleep state due to 'Idle Sleep' … Using Batt` at 10:04:20 -0700. On battery
//! the idle timeout here is one minute, so a lapse sleeps the Mac almost at
//! once. This dashboard already tracks which agents are working, which is the
//! fact a renewed per-session timeout is trying to approximate.
//!
//! # What it holds
//!
//! One `kIOPMAssertPreventUserIdleSystemSleep` assertion, taken while any local
//! row is [`Status::is_live_work`] and released when none is. It is the
//! assertion `caffeinate -i` takes: the display still sleeps, and the Mac still
//! sleeps for every reason that is not the idle timer — the lid, low battery,
//! thermal emergency, the Apple menu, an explicit Sleep.
//!
//! That bounded reach is why this one defaults on where [`crate::lid_awake`]
//! does not. Reaching the lid means the root-only, system-wide `pmset -a
//! disablesleep` kill switch, which suppresses the thermal and low-battery
//! sleeps too and therefore ships off behind a lease, a battery floor and four
//! recovery paths. An assertion needs none of that apparatus: the kernel
//! releases it when the owning process dies, so a panic, a `SIGKILL` and the
//! tray's `std::process::exit(0)` all clear it, and nothing survives a restart
//! to be cleared at startup. The two holds do not interact — `disablesleep`
//! outranks every assertion, so an armed lid veto simply makes this one moot
//! while it lasts.
//!
//! # Silence is what bounds a stuck hold
//!
//! A session whose process is alive but which has stopped producing anything is
//! indistinguishable from a wedged one, and the liveness reaper cannot help:
//! it tests whether the process exists, not whether it is making progress. So
//! the hold drops once every busy row has been silent for
//! [`Config::idle_awake_silence_ms`], measured against `AgentSession::updated`,
//! which the transcript watcher bumps on every write a live turn makes (p50
//! 7.7s between consecutive intra-turn writes, measured over 20,648 gaps).
//!
//! The window is deliberately generous, because the two errors cost different
//! things. Releasing early causes the exact sleep this module exists to
//! prevent; releasing late costs battery, and only until the low-battery sleep
//! that no assertion can suppress takes over. Two kinds of live turn are
//! genuinely silent on that clock and neither is rare:
//!
//! - **A single long tool call.** Nothing reaches the transcript between the
//!   entry issuing a `tool_use` and the entry consuming its result, so a long
//!   build or fetch freezes `updated` for its whole duration. Measured over 30
//!   days on this machine, 17 intra-turn gaps exceeded 15 minutes with an
//!   ordinary tool in flight.
//! - **Anything a subagent does.** Subagent transcripts are written under
//!   `subagents/**.jsonl`, which the watcher never opens, so a row held by a
//!   long workflow is silent here from start to finish. Nothing else bounds
//!   those either — `waiting_settle` excludes subagent-held WAITs from its own
//!   backstop for the same reason.
//!
//! A release that lands on one of those is corrected by the agent's next
//! output, which re-takes the assertion within a tick. A `null` or `0` window
//! holds for as long as the row stays busy.
//!
//! `pmset -g assertions` lists the live assertion against this process, which
//! is the only way to observe it from outside — unlike the lid veto, there is
//! no kernel property to read an assertion back from.
//!
//! macOS-only; every entry point is a no-op elsewhere.

use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::ConfigState;
use crate::state::{AgentSession, AppState};

/// Tick cadence. The hold is taken from `commands::emit_sessions_updated` the
/// moment a row goes busy, so this only has to notice the things that produce
/// no session event: the silence window expiring, and the flag being turned off
/// by a hand edit to `config.json`. Both are minutes-scale.
const POLL: Duration = Duration::from_secs(60);

/// How long to wait before retrying after a failed hold, so a Mac that refuses
/// the assertion does not log once per tick for as long as an agent is busy.
const HOLD_RETRY_BACKOFF_MS: i64 = 60_000;

// ---------------------------------------------------------------------------
// Pure decision layer (cross-platform so the tests run everywhere)
// ---------------------------------------------------------------------------

/// Everything the decision reads. Pure input — no clock, no IO — so the policy
/// is exercised without a Mac.
#[derive(Clone, Copy, Debug)]
struct Sample {
    enabled: bool,
    /// The newest `updated` among local rows that are live work, or `None` when
    /// none of them is. The newest wins so that one wedged row cannot drop the
    /// assertion out from under a sibling that is still producing output.
    newest_busy_update: Option<i64>,
    /// `None` or `0` holds for as long as a row stays busy.
    silence_ms: Option<u64>,
    now: i64,
}

impl Sample {
    /// Whether the assertion should be held right now.
    fn wants_hold(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(updated) = self.newest_busy_update else {
            return false;
        };
        match self.silence_ms {
            None | Some(0) => true,
            Some(window) => self.now.saturating_sub(updated) < window as i64,
        }
    }

    /// Why the assertion dropped, for the decision log.
    fn release_reason(&self) -> &'static str {
        if !self.enabled {
            "turned off"
        } else if self.newest_busy_update.is_none() {
            "no local agent is working"
        } else {
            "every working session has gone silent past the window; releasing so the Mac can sleep"
        }
    }
}

/// The newest `updated` among the rows that count as work, or `None` when none
/// does. Separate from [`Sample`] so the caller can feed it either the display
/// snapshot the emit path already holds or the tick's own read.
fn newest_busy_update(sessions: &[AgentSession]) -> Option<i64> {
    sessions.iter().filter(|s| s.status.is_live_work()).map(|s| s.updated).max()
}

// ---------------------------------------------------------------------------
// Tracked state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    /// The live assertion id, or `None` when nothing is held. `0` is
    /// `kIOPMNullAssertionID` and never stored.
    held: Option<u32>,
    /// Earliest retry after a failed hold — see [`HOLD_RETRY_BACKOFF_MS`].
    retry_after: i64,
}

#[derive(Default)]
pub struct IdleAwakeState {
    inner: std::sync::Mutex<Inner>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Re-decide from rows the caller already holds. Called from
/// `commands::emit_sessions_updated`, the chokepoint every status transition
/// flows through, so the assertion is taken on the transition itself rather
/// than at the next tick. Local rows only — a remote row is another machine's
/// work, and its device holds its own assertion.
pub fn sync(app: &AppHandle, sessions: &[AgentSession]) {
    decide(app, newest_busy_update(sessions));
}

/// Re-decide from the current state. For callers with no snapshot to hand — the
/// tick, and the tray toggle, which needs the change to take effect at once
/// because `config_watcher` skips the write it made itself.
pub fn reevaluate(app: &AppHandle) {
    let newest = app.try_state::<AppState>().and_then(|st| newest_busy_update(&st.snapshot()));
    decide(app, newest);
}

/// Periodic tick: expires the silence window and picks up a hand edit to
/// `config.json`, neither of which produces a session event to hang off.
pub fn spawn(app: AppHandle) {
    if !platform::supported() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("idle-awake watcher started");

        loop {
            ticker.tick().await;
            reevaluate(&app);
        }
    });
}

fn decide(app: &AppHandle, newest_busy_update: Option<i64>) {
    if !platform::supported() {
        return;
    }
    let (Some(cfg_state), Some(state)) = (app.try_state::<ConfigState>(), app.try_state::<IdleAwakeState>()) else {
        return;
    };
    let cfg = cfg_state.snapshot();
    let now = now_ms();

    let sample = Sample {
        enabled: cfg.idle_awake,
        newest_busy_update,
        silence_ms: cfg.idle_awake_silence_ms,
        now,
    };

    // The lock covers one IOKit call and nothing else. It must never be held
    // across a hop to the Tauri main thread: this runs inside
    // `commands::emit_sessions_updated`, which the main thread itself enters
    // through the sync commands and the tray menu.
    let mut inner = state.inner.lock().unwrap();

    if sample.wants_hold() {
        if inner.held.is_some() || now < inner.retry_after {
            return;
        }
        match platform::hold() {
            Ok(id) => {
                inner.held = Some(id);
                inner.retry_after = 0;
                tracing::info!(
                    decision = "idle_awake_hold",
                    assertion_id = id,
                    silence_ms = ?sample.silence_ms,
                    reason = "a local agent is working; holding off idle sleep so the turn isn't cut short",
                    "decision"
                );
            }
            Err(e) => {
                inner.retry_after = now + HOLD_RETRY_BACKOFF_MS;
                tracing::warn!(
                    decision = "idle_awake_hold",
                    error = %e,
                    retry_in_ms = HOLD_RETRY_BACKOFF_MS,
                    reason = "could not take the power assertion",
                    "decision"
                );
            }
        }
        return;
    }

    let Some(id) = inner.held.take() else { return };
    let silent_ms = sample.newest_busy_update.map(|t| now.saturating_sub(t));
    if let Err(e) = platform::release(id) {
        tracing::warn!(error = %e, "releasing the idle-sleep assertion failed; the kernel will drop it when this process exits");
    }
    tracing::info!(
        decision = "idle_awake_release",
        assertion_id = id,
        silent_ms = ?silent_ms,
        reason = sample.release_reason(),
        "decision"
    );
}

// ---------------------------------------------------------------------------
// Platform layer
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};

    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    /// `IOPMAssertionID`, a `uint32_t`.
    type IoPmAssertionId = u32;

    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    /// `kIOPMAssertionLevelOn`. **255, not 1** — the level is a bare `uint32_t`,
    /// so a plausible-looking wrong value compiles, links and silently asserts
    /// nothing.
    const ASSERTION_LEVEL_ON: u32 = 255;

    /// `kIOReturnSuccess`.
    const IO_RETURN_SUCCESS: c_int = 0;

    /// `kIOPMAssertPreventUserIdleSystemSleep`. A `CFSTR()` macro in
    /// `IOPMLib.h` rather than an exported symbol, so there is nothing to put in
    /// an extern block and the string is built at runtime.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";

    /// What `pmset -g assertions` shows against this process. `IOPMLib.h`
    /// requires a non-NULL name and caps it at 128 characters.
    const ASSERTION_NAME: &str = "A Claude Code agent is working";

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: CFAllocatorRef, cstr: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(assertion_type: CFStringRef, level: u32, name: CFStringRef, id: *mut IoPmAssertionId) -> c_int;
        fn IOPMAssertionRelease(id: IoPmAssertionId) -> c_int;
    }

    /// A CFString owned for the length of one call. `CFStringCreateWithCString`
    /// can return NULL, and `IOPMLib.h` says NULL is not a valid input for
    /// either argument, so construction is fallible rather than silently
    /// passing one on.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(s: &str) -> Option<Self> {
            let c = CString::new(s).ok()?;
            let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8) };
            // Constructed only on the non-null branch. `then_some(Self(r))`
            // takes its argument by *value*, so a null pointer would build the
            // wrapper anyway and drop it unused — running `Drop`, handing
            // `CFRelease` the NULL this guard exists to reject, and trapping
            // inside CoreFoundation instead of returning the `None` the
            // signature promises.
            if r.is_null() {
                None
            } else {
                Some(Self(r))
            }
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    pub(super) const fn supported() -> bool {
        true
    }

    pub(super) fn hold() -> Result<u32, String> {
        let kind = CfString::new(ASSERTION_TYPE).ok_or_else(|| "could not build the assertion-type string".to_string())?;
        let name = CfString::new(ASSERTION_NAME).ok_or_else(|| "could not build the assertion name".to_string())?;
        let mut id: IoPmAssertionId = 0;
        let rc = unsafe { IOPMAssertionCreateWithName(kind.0, ASSERTION_LEVEL_ON, name.0, &mut id) };
        if rc != IO_RETURN_SUCCESS {
            return Err(format!("IOPMAssertionCreateWithName returned {rc:#010x}"));
        }
        Ok(id)
    }

    pub(super) fn release(id: u32) -> Result<(), String> {
        let rc = unsafe { IOPMAssertionRelease(id) };
        if rc == IO_RETURN_SUCCESS {
            Ok(())
        } else {
            Err(format!("IOPMAssertionRelease returned {rc:#010x}"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) const fn supported() -> bool {
        false
    }
    pub(super) fn hold() -> Result<u32, String> {
        Err("idle-awake is macOS-only".to_string())
    }
    pub(super) fn release(_id: u32) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SetInput, Status};

    const MIN: i64 = 60_000;
    const WINDOW: u64 = 30 * 60_000; // 30 min

    fn sample(enabled: bool, newest_busy_update: Option<i64>, now: i64) -> Sample {
        Sample { enabled, newest_busy_update, silence_ms: Some(WINDOW), now }
    }

    /// Build rows through the real `apply_set`, so `updated` carries whatever
    /// the state machine actually writes rather than a hand-set field.
    fn rows(spec: &[(&str, Status, i64)]) -> Vec<AgentSession> {
        let state = AppState::new();
        for (id, status, updated) in spec {
            state.apply_set(
                SetInput {
                    id: (*id).into(),
                    status: *status,
                    label: None,
                    source: None,
                    model: None,
                    input_tokens: None,
                    dialog_entry: None,
                    waiting_backstop_armed: false,
                },
                *updated,
                &[],
                None,
            );
        }
        state.snapshot()
    }

    #[test]
    fn a_busy_session_holds_and_an_idle_board_does_not() {
        assert!(sample(true, Some(0), 0).wants_hold());
        assert!(!sample(true, None, 0).wants_hold(), "nothing working, nothing to protect");
    }

    #[test]
    fn the_flag_off_never_holds() {
        assert!(!sample(false, Some(0), 0).wants_hold());
    }

    #[test]
    fn a_silent_session_releases_only_past_the_window() {
        // A long single tool call or a subagent is silent on `updated`, so the
        // window has to outlast one comfortably.
        assert!(sample(true, Some(0), 29 * MIN).wants_hold(), "inside the window");
        assert!(!sample(true, Some(0), 30 * MIN).wants_hold(), "silent past the window");
    }

    #[test]
    fn a_live_sibling_keeps_the_hold_for_a_wedged_row() {
        // Two busy rows, one wedged an hour ago and one that wrote a second
        // ago: taking the newest is what stops the wedged one dropping the
        // assertion out from under work that is still producing output.
        let board = rows(&[("wedged", Status::Working, 0), ("live", Status::Working, 60 * MIN)]);
        let newest = newest_busy_update(&board);
        assert_eq!(newest, Some(60 * MIN));
        assert!(sample(true, newest, 60 * MIN + 1).wants_hold());
    }

    #[test]
    fn a_zero_or_absent_window_holds_indefinitely() {
        for silence_ms in [None, Some(0)] {
            let s = Sample { enabled: true, newest_busy_update: Some(0), silence_ms, now: 10 * 60 * MIN };
            assert!(s.wants_hold(), "{silence_ms:?} disables the silence cap");
        }
    }

    #[test]
    fn only_live_work_counts() {
        // Blocked is parked on the user and Done is finished — neither is work
        // that sleeping would suspend, so neither may hold the Mac awake.
        for status in [Status::Idle, Status::Blocked, Status::Done, Status::Error] {
            assert_eq!(newest_busy_update(&rows(&[("x", status, 5)])), None, "{status:?} must not hold");
        }
        for status in [Status::Working, Status::Waiting] {
            assert_eq!(newest_busy_update(&rows(&[("x", status, 5)])), Some(5), "{status:?} is live work");
        }
    }

    #[test]
    fn release_reason_names_the_cause() {
        assert_eq!(sample(false, Some(0), 0).release_reason(), "turned off");
        assert_eq!(sample(true, None, 0).release_reason(), "no local agent is working");
        assert!(sample(true, Some(0), 60 * MIN).release_reason().contains("gone silent"));
    }
}
