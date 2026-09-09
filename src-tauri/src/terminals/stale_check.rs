//! Catching a terminal surface that has stopped showing its session's status.
//!
//! **Why it exists.** The dashboard writes each row's status onto its tab title.
//! Where a terminal lets a user rename a tab, that tab keeps the typed name
//! forever and ignores every later write — and nothing about the write says so.
//! On Windows, `SetConsoleTitleW` keeps succeeding and `GetConsoleTitleW` keeps
//! reading the value back, because both touch the console object while the tab
//! shows something else. Measured live: `shown=ttt` against
//! `real=✋ what-is-next [78%]` — a session blocked on its user, behind a tab
//! saying nothing about it.
//!
//! **It is usually an accident.** Windows Terminal wires `TabViewItem::
//! DoubleTapped` straight to the renamer with no guard; `BeginRename` pre-fills
//! the box with the tab's current title and selects it; the box commits on
//! `LostFocus` as well as Enter. So a double-click on a tab followed by a click
//! anywhere else pins that tab to exactly the string this dashboard last wrote,
//! silently and permanently, with nothing typed. No setting disables the gesture
//! and nothing outside the terminal can undo it, so this reports and never
//! repairs.
//!
//! **Nothing here names a terminal.** Both strings arrive as
//! [`crate::terminals::FrontReading`]s from whichever
//! [`crate::terminals::TerminalAdapter`] this platform has, and the comparison is
//! the pure [`crate::terminals::read_front`]. This
//! module owns only what is genuinely cross-terminal: when to look, resolving a
//! reading to a row, deciding what a sequence of readings means, and writing the
//! flag.
//!
//! **The evidence is asymmetric, and the whole design turns on it.** A surface
//! showing a name that is not its session's title is strong evidence, once a
//! write has had time to land. A surface *agreeing* is weak: a tab stuck on
//! `⚪ web` agrees for free every time that row finishes and its title returns to
//! `⚪ web`, so an agreeing sample is consistent with a healthy tab and with a
//! stuck one alike. What separates them is **movement** — a following tab's
//! displayed name changes when the title we write changes, and a stuck one's
//! never does. So a suspicion is retracted when the displayed name is seen to
//! have *moved* and now matches, and never on agreement alone.
//!
//! **Edge-triggered, because no terminal announces this.** UIA raises no
//! property-changed event for a tab's name — measured over 15 real title changes
//! with handlers at subtree and desktop scope across seven event kinds — so
//! something has to ask. Asking is pinned to edges the app already has: a title
//! write, and a caption change. The second matters on its own, twice over: a
//! renamed tab's caption never moves, so switching *to* it is the only event
//! that says "look at this one now" — and when the user finally resets the tab,
//! that reset *is* a caption change, so the retraction arrives on its own edge
//! and needs no timer.
//!
//! **Never called while `terminal_title`'s console-attach lock is held.** Reading
//! a surface can be a cross-process call into a terminal's UI thread, measured
//! with multi-hundred-millisecond excursions, and that lock serialises every
//! title write in the process.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use super::{read_front, Reading};
use crate::config::ConfigState;
use crate::state::AppState;
use crate::terminal_title::{self, TerminalTitles};

/// Where a trigger says "something may have changed". Carries no payload: which
/// surfaces exist is the adapter's answer, not the caller's.
static CHECKS: OnceLock<Sender<()>> = OnceLock::new();

/// What one surface's state remembers between passes, for one row.
///
/// Records are kept per `(surface, row)` rather than per surface. A surface shows
/// one row at a time, so a surface-keyed record is silently *about* whichever row
/// was in front when it was written — and the next pass, after a tab switch,
/// judged it against a different one. That let a healthy tab retract a pinned
/// tab's flag, and it flipped between passes with the order the terminal happened
/// to enumerate in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspicion {
    /// The displayed name this surface is stuck on.
    ///
    /// The **only** string whose stability the report rests on. An earlier cut
    /// required the whole `(shown, real)` pair to repeat, which cannot happen for
    /// a busy row: `real` is the title this dashboard writes, and it moves on
    /// every glyph change and every `[N%]` tick. So the pair never repeated, the
    /// suspicion re-recorded on every pass, a working session's pinned tab was
    /// never reported at all, and the checker re-read every terminal window every
    /// few seconds for as long as that row stayed busy.
    pub shown: String,
    /// When this displayed name was first seen to be stuck, never refreshed by a
    /// repeat — the clock measures the fault rather than the last time it was
    /// noticed.
    pub since: i64,
    /// Whether it has already been reported, so a fault left standing overnight
    /// writes one warn line rather than one per trigger into the file the
    /// `investigate` skill greps.
    pub reported: bool,
}

/// What a pass does about one surface. Pure so the deciding half is testable:
/// every defect this module has had lived here rather than in the comparison,
/// and `check` could not be exercised at all because it takes an `AppHandle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing new was established. Keep whatever is recorded, change no flag.
    Hold,
    /// Start or restart the clock on this displayed name.
    Record,
    /// Confirmed. Set the flag, and log if this is the first time.
    Report,
    /// The surface is following, and has been seen to move to prove it. Drop the
    /// record and clear the flag.
    Clear,
}

/// Decide one surface's fate for one row, from what was recorded and what is on
/// screen now.
///
/// `prev` must already be scoped to this row; the caller keys its map on
/// `(surface, row)` so a reading can only ever meet a record about itself.
///
/// `diverged` is [`Reading::Diverged`] — the surface is showing something that is
/// not its session's title. `shown` is the displayed name in either case.
pub fn stale_step(prev: Option<&Suspicion>, shown: &str, diverged: bool, now: i64, grace: i64) -> Step {
    if !diverged {
        return match prev {
            // Agreement on the *same* name proves nothing: this is the string the
            // surface was already stuck on, and the row's title has merely come
            // back round to it. Retracting here is how a tab pinned at a status
            // the row keeps returning to shed its flag on every turn — and since
            // re-setting the flag re-stamps the instant, the Telegram alert's
            // delay restarted each time and could never mature.
            Some(p) if p.shown == shown => Step::Hold,
            // Either the name moved (a following tab, proven by the movement — a
            // reset tab title lands here) or nothing was recorded. Both are
            // reasons to hold no flag: an unretractable flag is worse than a late
            // one, and this is the arm that retracts what the caption oracle set.
            _ => Step::Clear,
        };
    }
    match prev {
        Some(p) if p.shown == shown => {
            // Re-assert rather than assume. `terminal_stale_at` has other writers
            // — toggling `terminal_titles` off calls `clear_all_terminal_stale` —
            // and a latch that never re-stated its verdict would let the badge be
            // retired behind its back. Setting a flag already set changes no
            // timestamp, so this costs nothing and restarts no alert clock.
            if p.reported || now - p.since >= grace {
                Step::Report
            } else {
                Step::Hold
            }
        }
        // A name we have not seen stuck before. One sample cannot tell a stuck tab
        // from a write still in flight or a rename box open mid-edit, so start its
        // clock rather than accusing it.
        _ => Step::Record,
    }
}

/// Drop the *unconfirmed* records this surface holds for anything other than the
/// row it is showing now. `row` is `None` when the surface gave a reading this
/// dashboard could not name a row from, in which case none of its unconfirmed
/// records is judgeable and all of them go.
///
/// **A reported record always survives**, and that exception is the whole
/// correctness of this function. Its only job is to stop an unreachable record
/// keeping `seen.values().any(|s| !s.reported)` true, which arms the self-wake on
/// every pass and turns an edge-triggered check into a permanent UIA poll — and
/// that condition counts unconfirmed records alone. Pruning a *reported* one
/// bought nothing and cost everything: that record is the only thing
/// `stale_step`'s `Some(p) if p.shown == shown => Hold` has to recognise a
/// coincidental agreement by, so dropping it let the next return to the pinned
/// status read as `Following` with no history, clear the flag, delete the
/// outstanding Telegram message, and re-stamp the alert clock on the following
/// confirmation. That is verbatim the starvation the single-writer rule exists to
/// prevent, reintroduced from inside.
///
/// Pure, so it can be tested: `check` takes an `AppHandle` and cannot be
/// exercised, and this map's lifecycle is where two review rounds running found
/// their blocking defects.
pub fn prune_other_rows(seen: &mut HashMap<(String, String), Suspicion>, surface: &str, row: Option<&str>) {
    seen.retain(|(s, r), sus| sus.reported || s != surface || Some(r.as_str()) == row);
}

/// Added to the grace when a standing suspicion schedules its own second look,
/// so the second reading is comfortably outside the window the first one had to
/// clear rather than racing its boundary.
const SECOND_LOOK_MARGIN_MS: u64 = 500;

/// How many times the worker asks whether this terminal can be read at all before
/// concluding it cannot, and how long it waits between asks.
///
/// A single answer will not do. `front_readings` returns `None` for *any* failure
/// to look, and the dashboard and the terminal both start at login in no fixed
/// order, so the first ask routinely arrives before there is anything to ask —
/// the same race `session_restore` retries through. Standing down on one such
/// answer would disable the feature for the life of the process on a platform
/// that supports it perfectly.
const CAPABILITY_ATTEMPTS: u32 = 5;
const CAPABILITY_RETRY_MS: u64 = 3_000;

/// Ask for the terminal's surfaces to be looked at shortly.
///
/// Non-blocking. It is called from `terminal_title::sync`, which runs inside the
/// axum hook handler and inside the transcript-watcher thread, and from an
/// adapter's watch consumer — none of which has any business waiting on a
/// terminal.
pub fn request() {
    if let Some(tx) = CHECKS.get() {
        let _ = tx.send(());
    }
}

/// Start the worker, if this platform has a terminal to ask. A no-op if called
/// twice.
///
/// Two ways it declines to run, and they are different questions. `for_platform`
/// answering `None` means the platform has no adapter at all. An adapter that
/// does not implement [`super::TerminalAdapter::front_readings`] means the
/// terminal cannot be asked what it is displaying — today that is agterm — and
/// the worker stands down after [`CAPABILITY_ATTEMPTS`] rather than parking on
/// the channel forever, reading nothing on every trigger, behind a log line
/// announcing a checker that had started.
///
/// Deliberately not gated on `terminal_titles`: that flag hot-reloads from the
/// tray and `check` re-reads it every pass, so gating the spawn on its start-time
/// value would leave a user who enabled titles later with checks requested and
/// nothing to receive them.
pub fn spawn(app: AppHandle) {
    let Some(adapter) = super::for_platform(&app) else {
        return;
    };
    let (tx, rx) = mpsc::channel::<()>();
    if CHECKS.set(tx).is_err() {
        return;
    }
    let terminal = adapter.name();
    std::thread::spawn(move || {
        adapter.prepare_reader();
        if !can_read(adapter.as_ref(), terminal) {
            return;
        }
        tracing::info!(terminal, "stale surface checker started");
        // Per (surface, row), the displayed name last seen stuck there. A report
        // needs the same name twice over the grace, which is what absorbs a
        // reading taken while a write was still propagating, a rename box open
        // mid-edit, and a repaint that lost a race.
        let mut seen: HashMap<(String, String), Suspicion> = HashMap::new();
        while rx.recv().is_ok() {
            // Let a write reach the surface, and coalesce the burst one emit
            // makes into a single look.
            std::thread::sleep(Duration::from_millis(terminal_title::grace_ms() as u64));
            while rx.try_recv().is_ok() {}
            let pending = check(&app, adapter.as_ref(), terminal, &mut seen);
            // A divergence this pass could not yet confirm fetches its own second
            // look. Every other trigger is an *event* — a title write, a caption
            // change — and a pinned surface on a quiet board produces neither: its
            // caption is frozen by definition, and `sync` only runs from
            // `emit_sessions_updated`, which nothing calls on a timer. So a first
            // divergence would otherwise be recorded and then simply forgotten.
            //
            // It terminates because the wake is armed by what this pass *saw*, not
            // by what the map still holds. The name it waits on is the stuck one,
            // which by definition does not move, so the second look either
            // confirms it or finds it gone. Asking the map instead armed on
            // records no pass could reach — a surface that keeps abstaining, or a
            // row whose title has come back round to the string its tab is stuck
            // on and so answers `Hold` — and neither is waiting for anything, so
            // the wake re-armed on every pass forever.
            if pending {
                let tx = CHECKS.get().cloned();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(terminal_title::grace_ms() as u64 + SECOND_LOOK_MARGIN_MS));
                    if let Some(tx) = tx {
                        let _ = tx.send(());
                    }
                });
            }
        }
    });
}

/// Whether this terminal can be asked what it is displaying, retried through the
/// login race. See [`CAPABILITY_ATTEMPTS`].
fn can_read(adapter: &dyn super::TerminalAdapter, terminal: &'static str) -> bool {
    for attempt in 1..=CAPABILITY_ATTEMPTS {
        if adapter.front_readings().is_some() {
            return true;
        }
        if attempt < CAPABILITY_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(CAPABILITY_RETRY_MS));
        }
    }
    tracing::debug!(terminal, attempts = CAPABILITY_ATTEMPTS, "this terminal cannot be asked what it is displaying; stale surface checks stand down");
    false
}

/// Returns whether this pass left a divergence unconfirmed, i.e. whether a second
/// look would settle something.
///
/// Read from the pass rather than from `seen`, which is the difference between a
/// bounded self-wake and a permanent poll. Scanning the map for any unconfirmed
/// record armed the wake on records no pass could reach — a surface that keeps
/// abstaining, a row whose title has come back round to the string its tab is
/// stuck on and so answers `Hold` — and neither is waiting for a second look,
/// because looking again produces the same answer. What this pass actually saw
/// cannot have that problem.
fn check(app: &AppHandle, adapter: &dyn super::TerminalAdapter, terminal: &'static str, seen: &mut HashMap<(String, String), Suspicion>) -> bool {
    let Some(titles) = app.try_state::<TerminalTitles>() else { return false };
    // With titles off, `sync` has blanked every tab and cleared every flag, so
    // there is nothing a surface could disagree with. Checked here rather than
    // relied on indirectly: a trigger queued just before the toggle would
    // otherwise run afterwards and could re-set a flag `clear_all_terminal_stale`
    // had just dropped, with no later trigger able to clear it again.
    if !app.try_state::<ConfigState>().map(|c| c.snapshot().terminal_titles).unwrap_or(true) {
        seen.clear();
        return false;
    }
    let now = crate::commands::now_ms();
    let grace = terminal_title::grace_ms();
    // `None` is a failure to look, so every standing suspicion is left exactly as
    // it was: dropping them here would let an unreadable pass silently clear a
    // genuine one, and recording one would judge a surface nobody saw.
    let Some(readings) = adapter.front_readings() else {
        tracing::debug!(terminal, decision = "surface_check_abstained", reason = "terminal_unreadable", "the terminal gave no answer this pass");
        return false;
    };
    // Surfaces the terminal no longer reports cannot be re-judged, so their
    // records are dropped rather than left to outlive them. The row's flag is
    // deliberately *not* cleared with them: a window that closed took its session
    // with it, and a tab torn into a new window is still stuck.
    seen.retain(|(surface, _), _| readings.iter().any(|r| &r.surface == surface));

    let mut pending = false;
    for r in &readings {
        // The row is named by the session's own title — a string this dashboard
        // wrote — never by what the surface displays, which the user may have
        // overwritten. That one discipline is what lets the rest be a comparison
        // rather than an inference, and it is structural here because
        // `Reading::Following` carries the real title too rather than leaving the
        // caller to reach for `shown` on the strength of the two being equal.
        let (shown, real, diverged) = match read_front(r) {
            Reading::Following(real) => (real.clone(), real, false),
            Reading::Diverged { shown, real } => (shown, real, true),
            // Two different things wear this name, and the difference decides
            // whether the records for this surface may be touched.
            //
            // `surface_answered` means the surface told us something positive — it
            // holds several panes, or none — so whatever row we had recorded for it
            // is known not to be in front, and the record is unreachable. Prune it.
            //
            // Otherwise the read simply failed, and a failure to look is never
            // grounds to discard what was already observed: dropping the record
            // there restarts the two-sample clock and, worse, cancels the second
            // look it was waiting for, so a transient COM failure landing on that
            // look loses a real fault on a row that writes no titles and therefore
            // re-triggers nothing.
            Reading::Unknown { reason, surface_answered } => {
                tracing::debug!(terminal, decision = "surface_check_abstained", surface = %r.surface, reason, surface_answered, "no verdict on this surface");
                if surface_answered {
                    prune_other_rows(seen, &r.surface, None);
                }
                continue;
            }
        };
        let named = titles.row_for_title(&real, &r.surface);
        // Pruned on a *reading*, above the gates below, which is the distinction
        // the `Unknown` arm draws. Getting here means the surface answered: we
        // know which row is in front, or we know it is one this dashboard does not
        // title. Either way its records for other rows are unreachable, and this
        // is the only path that can say so. Below the `continue`s it never ran on
        // the case that creates the orphan.
        prune_other_rows(seen, &r.surface, named.as_ref().map(|(row, _)| row.as_str()));
        let Some((row, written_at)) = named else {
            tracing::debug!(terminal, decision = "surface_check_abstained", surface = %r.surface, reason = "not_our_title", "the surface holds a session this dashboard does not title, or names it ambiguously");
            continue;
        };
        // A write still propagating is the one race the comparison cannot see:
        // the pane's title moves before the tab's name does, so a reading taken
        // in that window disagrees about a healthy tab. Measured 5.8 to 90.8 ms
        // end to end; the grace is ~20x that ceiling.
        if now - written_at < grace {
            tracing::debug!(terminal, decision = "surface_check_abstained", surface = %r.surface, reason = "within_grace", "the title for this row was written too recently to judge");
            continue;
        }

        let key = (r.surface.clone(), row.clone());
        match stale_step(seen.get(&key), &shown, diverged, now, grace) {
            // `Hold` on a *divergence* is the two-sample rule waiting out the
            // grace, so it is pending. `Hold` on an agreement is the coincidence
            // guard, which another look cannot advance.
            Step::Hold => pending |= diverged,
            Step::Record => {
                pending = true;
                seen.insert(key, Suspicion { shown, since: now, reported: false });
            }
            Step::Report => {
                let first = seen.get(&key).is_some_and(|s| !s.reported);
                if let Some(sus) = seen.get_mut(&key) {
                    sus.reported = true;
                }
                if first {
                    tracing::warn!(
                        terminal,
                        decision = "surface_stale",
                        surface = %r.surface,
                        chat_id = %row,
                        remedy = adapter.stale_remedy(),
                        "this terminal surface is showing a name that is not its session's status, twice over the grace window"
                    );
                }
                set_flag(app, &row, true, now);
            }
            Step::Clear => {
                // One row can own two tabs in one surface — a `--fork-session
                // --resume` migration leaves two live sessions on one cwd-derived
                // row — and the seam offers no tab identity to tell them apart, so
                // switching to the healthy sibling can read as "the name moved and
                // now matches" and clear a fault still sitting on the other tab.
                //
                // Gating this on the registry counting exactly one session was
                // tried and reverted. `Step::Report` has no such gate, so the flag
                // could be set in a state where it could never be cleared: the
                // user resets the tab exactly as the alert instructs and the badge
                // stays for the life of the process. Between two errors we cannot
                // tell apart, take the self-correcting one — a sibling's clear is
                // undone by the next divergence, while an unclearable badge is
                // undone by nothing.
                if let Some(prev) = seen.remove(&key) {
                    if prev.reported {
                        tracing::info!(terminal, decision = "surface_following", surface = %r.surface, shown = %shown, "the surface is showing its session's title again");
                    }
                }
                set_flag(app, &row, false, now);
            }
        }
    }
    pending
}

fn set_flag(app: &AppHandle, chat_id: &str, stale: bool, now: i64) {
    if app.try_state::<AppState>().is_some_and(|s| s.set_terminal_stale(chat_id, stale, now)) {
        crate::commands::emit_sessions_updated(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: i64 = 2_000;

    fn sus(shown: &str, since: i64, reported: bool) -> Suspicion {
        Suspicion { shown: shown.into(), since, reported }
    }

    #[test]
    fn a_first_divergence_is_recorded_not_reported() {
        assert_eq!(stale_step(None, "ttt", true, 1_000, G), Step::Record);
    }

    #[test]
    fn the_same_displayed_name_reports_once_the_grace_has_passed() {
        let p = sus("ttt", 0, false);
        assert_eq!(stale_step(Some(&p), "ttt", true, G - 1, G), Step::Hold);
        assert_eq!(stale_step(Some(&p), "ttt", true, G, G), Step::Report);
    }

    #[test]
    fn the_session_title_moving_underneath_does_not_restart_the_clock() {
        // The defect this rule replaced. Stability used to be required of the
        // whole `(shown, real)` pair, but `real` is the title this dashboard
        // writes: it moves on every glyph change and every `[N%]` tick. A busy
        // row's pinned tab therefore never repeated a pair, so it re-recorded on
        // every pass, was never reported, and kept the checker re-reading every
        // terminal window for as long as the row stayed busy. Only the stuck name
        // has to hold still, and it does by definition.
        let p = sus("ttt", 0, false);
        assert_eq!(stale_step(Some(&p), "ttt", true, G, G), Step::Report, "the row's own title is not part of the comparison");
    }

    #[test]
    fn a_reported_fault_keeps_re_asserting_its_verdict() {
        // `terminal_stale_at` has other writers — toggling `terminal_titles` off
        // calls `clear_all_terminal_stale` — so a latch that answered Hold and
        // stopped re-stating itself would retire the badge for good behind its
        // back. The caller logs once and sets the flag on every confirmation.
        let p = sus("ttt", 0, true);
        assert_eq!(stale_step(Some(&p), "ttt", true, 100 * G, G), Step::Report);
    }

    #[test]
    fn a_different_displayed_name_restarts_the_clock_rather_than_confirming_the_old_one() {
        // Without this, two unrelated transients could combine into a report.
        let p = sus("ttt", 0, false);
        assert_eq!(stale_step(Some(&p), "other", true, 10 * G, G), Step::Record);
    }

    #[test]
    fn agreeing_on_the_name_it_is_stuck_on_proves_nothing_and_holds() {
        // A tab pinned at `⚪ web` agrees for free every time that row finishes
        // and its title comes back round to `⚪ web`. Retracting there is how the
        // flag flipped on every turn — and since re-setting it re-stamps the
        // instant, the Telegram alert's delay restarted each time and could never
        // mature.
        let p = sus("⚪ web", 0, true);
        assert_eq!(stale_step(Some(&p), "⚪ web", false, 100 * G, G), Step::Hold);
    }

    #[test]
    fn agreeing_after_the_name_moved_clears_at_once() {
        // Movement is the positive evidence: a following tab's displayed name
        // changes when the title we write changes, and a stuck one's never does.
        // This is the arm a reset tab title lands on, and it needs no grace —
        // nothing in flight can make a stuck tab move.
        let p = sus("ttt", 0, true);
        assert_eq!(stale_step(Some(&p), "🟢 what-is-next", false, G / 4, G), Step::Clear);
    }

    fn seen_with(entries: &[(&str, &str, bool)]) -> HashMap<(String, String), Suspicion> {
        entries.iter().map(|(s, r, rep)| ((s.to_string(), r.to_string()), sus("ttt", 0, *rep))).collect()
    }

    #[test]
    fn a_surface_keeps_only_the_row_it_is_showing() {
        // The record for a row this surface no longer shows can never be re-read:
        // only the front tab is judged. Left standing while unreported it keeps
        // the self-wake armed on every pass, which is how an edge-triggered check
        // became a permanent UIA poll. A record for the *same* row survives, and
        // so does another surface's.
        let mut seen = seen_with(&[("w1", "a", false), ("w1", "b", true), ("w2", "a", false)]);
        prune_other_rows(&mut seen, "w1", Some("b"));
        let mut keys: Vec<_> = seen.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec![("w1".to_string(), "b".to_string()), ("w2".to_string(), "a".to_string())]);
    }

    #[test]
    fn a_confirmed_record_survives_the_prune() {
        // The whole correctness of the prune. A reported record is what
        // `stale_step` recognises a coincidental agreement by, so dropping it
        // lets the next return to the pinned status read as Following with no
        // history, clear the flag and delete the outstanding alert. The prune
        // exists only to stop an *unconfirmed* orphan arming the self-wake, and
        // that condition counts unconfirmed records alone.
        let mut seen = seen_with(&[("w1", "pinned", true), ("w1", "provisional", false)]);
        prune_other_rows(&mut seen, "w1", Some("other"));
        let keys: Vec<_> = seen.keys().map(|(_, r)| r.as_str()).collect();
        assert_eq!(keys, vec!["pinned"], "confirmed evidence must outlive a visit to another tab");
    }

    #[test]
    fn an_unnameable_surface_drops_only_its_unconfirmed_records() {
        // `None` is "this surface gave a reading we could not name a row from",
        // so none of its unconfirmed records is judgeable this pass. This is the
        // path the prune never used to reach, because it sat below the
        // abstention `continue`s that create the orphan.
        let mut seen = seen_with(&[("w1", "a", false), ("w1", "b", true), ("w2", "c", false)]);
        prune_other_rows(&mut seen, "w1", None);
        let mut keys: Vec<_> = seen.keys().map(|(s, r)| format!("{s}/{r}")).collect();
        keys.sort();
        assert_eq!(keys, vec!["w1/b".to_string(), "w2/c".to_string()]);
    }

    #[test]
    fn pruning_a_surface_with_nothing_else_recorded_keeps_its_own_entry() {
        let mut seen = seen_with(&[("w1", "a", true)]);
        prune_other_rows(&mut seen, "w1", Some("a"));
        assert_eq!(seen.len(), 1, "the row in front must keep its accumulated evidence");
    }

    #[test]
    fn a_healthy_surface_with_nothing_recorded_clears_at_once() {
        // A flag nobody can retract is worse than a late one, and this is the arm
        // that retracts one the caption oracle set.
        assert_eq!(stale_step(None, "🟢 web", false, 0, G), Step::Clear);
    }
}
