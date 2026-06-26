//! Backstop for the one cancellation the transcript can't see.
//!
//! [`crate::log_watcher`] demotes a `Working` row to `Idle` when it sees the
//! "[Request interrupted by user]" transcript marker — but Claude Code only
//! writes that marker when it interrupts *active* work (a running tool, an
//! in-progress response). An **instant** Esc (submit a prompt, cancel before
//! Claude emits anything) writes nothing to the transcript and fires no hook,
//! so the row's only event was the `UserPromptSubmit` that set `Working`. There
//! is no event-driven signal that the turn ended.
//!
//! This probe covers that gap on Windows by reading the session's terminal
//! screen (the same console the tab title is written to — see
//! [`crate::terminal_title`]). It is **positive-only and fail-safe**: it
//! demotes solely when it can *see Claude's idle prompt* — the input box (its
//! long `─` border) is on screen with no "esc to interrupt" / permission
//! prompt. If the console can't be read, or the screen doesn't positively show
//! the prompt (wrong console, blank read, a layout it doesn't recognise), it
//! does nothing. So it can never demote a genuinely-working row; the worst
//! failure mode is "doesn't fire", which just falls back to the next-prompt
//! recovery. (The input-box border is the anchor rather than a footer hint
//! like "? for shortcuts" because that hint is absent in auto-accept mode.)
//!
//! The screen alone isn't enough: queuing a *second* prompt while a turn is
//! still running grows the input box, which can scroll the "esc to interrupt"
//! footer past the [`TAIL_LINES`] window and make a busy turn read as idle. Two
//! corroborations guard against that. First, a *pending queued prompt* is
//! detected directly: Claude Code writes a `queue-operation` transcript record
//! (`enqueue` when the user queues a prompt mid-turn, `remove` when it is later
//! dequeued to run), so an outstanding `enqueue` means the input box is occupied
//! and the bordered "idle prompt" reading is spurious — [`has_pending_queued_command`]
//! suppresses the demote outright while one is live. Second, even with no queued
//! prompt, a demote is corroborated against the transcript mtime — when the idle
//! streak begins we latch the file's mtime, and any write past that baseline
//! means the turn is still alive (re-arm, don't demote). Only an unbroken idle
//! streak over a *quiet*, unqueued transcript demotes; an unreadable transcript
//! is treated as "can't confirm" and never demotes.
//!
//! Windows-only: reading a console's screen buffer is a Win32 facility with no
//! macOS equivalent. [`crate::terminal_title::read_console_screen`] returns
//! `None` off-Windows, so the loop never demotes there.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use tauri::{AppHandle, Manager};

use crate::commands::{emit_sessions_updated, now_ms};
use crate::config::ConfigState;
use crate::log_watcher::WatcherRegistry;
use crate::state::{AppState, Status};
use crate::terminal_title::{read_console_screen, TerminalTitles};

/// Substrings that mean the session is occupied. `esc to interrupt` is shown
/// the whole time the model generates; `Do you want to` is the body of every
/// tool-permission prompt (allow / edit / proceed), during which the row stays
/// `Working` with no spinner.
const BUSY_MARKERS: &[&str] = &["esc to interrupt", "Do you want to"];

/// The input box is framed by a long horizontal rule. A run of this many `─`
/// (U+2500) in the tail is the positive anchor that we're looking at Claude's
/// prompt — present in every mode, busy or idle, and absent from a blank or
/// wrong console. Plain prose never strings this many together.
const PROMPT_BORDER: &str = "────────────────────"; // 20 × U+2500

/// Only the last N visible rows are searched — the footer and input box live
/// at the bottom, so this avoids matching a marker quoted in scrollback above.
const TAIL_LINES: usize = 15;

/// Consecutive idle reads required before demoting, to ride out a one-off
/// transient read at the [`POLL`] cadence — notably the brief frame right
/// after a prompt is submitted, before the footer switches to "esc to
/// interrupt". With [`POLL`] at 1s this is a ~1–2s demote latency.
const IDLE_STREAK_TO_DEMOTE: u32 = 2;

/// How often to sample working sessions' consoles. Kept at 1s for a snappy
/// demote on an instant cancel; each tick only reads consoles for rows that
/// are actually `Working`, so an idle dashboard does no console work.
const POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Screen {
    Busy,
    Idle,
    Unknown,
}

/// Classify a console screen dump by its bottom [`TAIL_LINES`] non-blank rows.
fn classify(screen: &str) -> Screen {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    let tail = lines[start..].join("\n");
    if BUSY_MARKERS.iter().any(|m| tail.contains(m)) {
        Screen::Busy
    } else if tail.contains(PROMPT_BORDER) {
        Screen::Idle
    } else {
        Screen::Unknown
    }
}

/// Read a transcript file's last-modified time, or `None` if it can't be
/// stat'd (missing, permission, or off a path the watcher isn't tracking).
fn transcript_mtime(path: PathBuf) -> Option<SystemTime> {
    std::fs::metadata(&path).and_then(|m| m.modified()).ok()
}

/// Bytes of transcript tail scanned for queue operations. A still-pending
/// `enqueue` was written during the current (running) turn, so it sits among the
/// newest records — the tail is enough, and far cheaper than reading a multi-MB
/// transcript on every demote check.
const QUEUE_SCAN_TAIL_BYTES: u64 = 64 * 1024;

/// True when the transcript shows a user prompt currently sitting queued in the
/// input box — an `enqueue` queue-operation with no matching `remove` yet. While
/// one is outstanding the input box is occupied, so the bordered-input "idle
/// prompt" the screen classifier keys on is really a *busy* turn whose "esc to
/// interrupt" footer scrolled off the tail. Reading the file tail (not the whole
/// transcript) suffices. Any read failure → `false`; the mtime guard downstream
/// still backstops, so this never causes a demote on its own.
fn has_pending_queued_command(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else { return false };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return false };
    if file.seek(SeekFrom::Start(len.saturating_sub(QUEUE_SCAN_TAIL_BYTES))).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    // Lossy decode so a tail cut landing mid-codepoint can't error the whole
    // read (the truncated leading line just fails to parse and is skipped).
    queued_pending(&String::from_utf8_lossy(&buf))
}

/// Pure net-pending decision over a transcript tail: more `enqueue` than
/// `remove` queue-operation records means a prompt is still queued. Matched
/// structurally (a parsed `queue-operation` record), so prose mentioning the
/// words can't trip it, and a truncated leading line from the tail cut simply
/// fails to parse and is skipped.
fn queued_pending(tail: &str) -> bool {
    let mut net: i32 = 0;
    for line in tail.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("queue-operation") {
            continue;
        }
        match v.get("operation").and_then(|o| o.as_str()) {
            Some("enqueue") => net += 1,
            Some("remove") => net -= 1,
            _ => {}
        }
    }
    net > 0
}

/// What one idle screen read resolves to, once corroborated against the
/// transcript. Returned by [`idle_step`]; the caller owns the bookkeeping maps.
#[derive(PartialEq, Debug)]
enum IdleStep {
    /// First idle read of a streak — caller latches `mtime` as the baseline.
    Latch,
    /// The transcript advanced past the baseline since we began suspecting
    /// idle — the turn is still running (e.g. a queued second prompt scrolled
    /// the busy footer off-screen). Caller drops the streak and re-arms.
    Rearm,
    /// Idle persisting with no new transcript activity — caller demotes.
    Demote,
    /// Idle persisting but short of the demote streak — caller stores the count.
    Hold(u32),
}

/// Pure decision for an idle read. `baseline` is the latched transcript mtime
/// (`None` until the first idle read of a streak); `mtime` is the transcript's
/// current mtime. A write strictly past the baseline re-arms; otherwise an
/// unbroken streak of [`IDLE_STREAK_TO_DEMOTE`] quiet idle reads demotes.
fn idle_step(prior_streak: u32, baseline: Option<SystemTime>, mtime: SystemTime) -> IdleStep {
    match baseline {
        None => IdleStep::Latch,
        Some(b) if mtime > b => IdleStep::Rearm,
        Some(_) if prior_streak + 1 >= IDLE_STREAK_TO_DEMOTE => IdleStep::Demote,
        Some(_) => IdleStep::Hold(prior_streak + 1),
    }
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // chat_id → consecutive idle reads.
        let mut idle_streak: HashMap<String, u32> = HashMap::new();
        // chat_id → transcript mtime latched when the idle streak began. A
        // write past this baseline means the turn is still alive (e.g. a second
        // prompt queued mid-turn scrolled the busy footer off the tail), so we
        // re-arm rather than demote.
        let mut suspect_mtime: HashMap<String, SystemTime> = HashMap::new();
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("idle probe started");

        loop {
            ticker.tick().await;

            let Some(cfg_state) = app.try_state::<ConfigState>() else { continue };
            if !cfg_state.snapshot().detect_cancelled_turns {
                idle_streak.clear();
                suspect_mtime.clear();
                continue;
            }
            let Some(app_state) = app.try_state::<AppState>() else { continue };
            let Some(titles) = app.try_state::<TerminalTitles>() else { continue };
            let Some(registry) = app.try_state::<WatcherRegistry>() else { continue };

            let sessions = app_state.snapshot();
            // Drop bookkeeping for any session no longer Working.
            idle_streak.retain(|id, _| {
                sessions.iter().any(|s| s.id == *id && s.status == Status::Working)
            });
            suspect_mtime.retain(|id, _| idle_streak.contains_key(id));

            for s in sessions.iter().filter(|s| s.status == Status::Working) {
                let candidates = titles.candidates(&s.id);
                if candidates.is_empty() {
                    continue;
                }
                let Some(screen) = read_console_screen(&candidates) else {
                    continue; // unreadable this tick — not evidence of idle
                };
                match classify(&screen) {
                    Screen::Idle => {
                        let path = registry.current_path(&s.id);
                        // A queued prompt occupies the input box, so its border
                        // reads as the idle prompt while the turn is still alive
                        // (the "esc to interrupt" footer scrolled off the tail).
                        // Suppress the demote outright while one is pending.
                        if path.as_deref().map(has_pending_queued_command).unwrap_or(false) {
                            idle_streak.remove(&s.id);
                            suspect_mtime.remove(&s.id);
                            continue;
                        }
                        // Corroborate the idle-looking screen against the
                        // transcript. With no readable transcript we can't prove
                        // the turn ended, so stay fail-safe and never demote.
                        let Some(mtime) = path.and_then(transcript_mtime) else {
                            idle_streak.remove(&s.id);
                            suspect_mtime.remove(&s.id);
                            continue;
                        };
                        let prior = idle_streak.get(&s.id).copied().unwrap_or(0);
                        match idle_step(prior, suspect_mtime.get(&s.id).copied(), mtime) {
                            IdleStep::Latch => {
                                suspect_mtime.insert(s.id.clone(), mtime);
                                idle_streak.insert(s.id.clone(), 1);
                            }
                            IdleStep::Rearm => {
                                idle_streak.remove(&s.id);
                                suspect_mtime.remove(&s.id);
                            }
                            IdleStep::Hold(n) => {
                                idle_streak.insert(s.id.clone(), n);
                            }
                            IdleStep::Demote => {
                                if let Some(status) = app_state.revert_cancelled_turn(&s.id, now_ms()) {
                                    tracing::debug!(
                                        chat_id = %s.id,
                                        decision = "revert_cancelled",
                                        status = ?status,
                                        reason = "terminal shows Claude's idle prompt and the transcript stopped advancing (instant Esc-cancel, no transcript marker); reverted to pre-prompt status",
                                        "decision"
                                    );
                                    emit_sessions_updated(&app);
                                }
                                idle_streak.remove(&s.id);
                                suspect_mtime.remove(&s.id);
                            }
                        }
                    }
                    // Busy or an unrecognised screen both reset the streak —
                    // only an unbroken run of positive idle reads demotes.
                    Screen::Busy | Screen::Unknown => {
                        idle_streak.remove(&s.id);
                        suspect_mtime.remove(&s.id);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic Claude idle prompt: status line, bordered input box, footer.
    // Mirrors the captured auto-mode idle screen (no "for shortcuts").
    fn idle_screen() -> String {
        format!("✻ Cooked for 2m 19s\n{b}\n❯\n{b}\n  ⏵⏵ auto mode on · ← for agents · ↓ to manage", b = PROMPT_BORDER)
    }

    #[test]
    fn generating_footer_is_busy() {
        let screen = format!("{b}\n❯\n{b}\n  ⏵⏵ auto mode on · esc to interrupt · ↓ to manage", b = PROMPT_BORDER);
        assert_eq!(classify(&screen), Screen::Busy);
    }

    #[test]
    fn permission_prompt_is_busy() {
        let screen = "Bash command\n\nDo you want to proceed?\n  1. Yes\n  2. No";
        assert_eq!(classify(screen), Screen::Busy);
    }

    #[test]
    fn idle_prompt_is_idle() {
        assert_eq!(classify(&idle_screen()), Screen::Idle);
    }

    #[test]
    fn busy_wins_even_with_border() {
        // Generating screen also has the input-box border — busy must win.
        let screen = format!("{b}\n❯\n{b}\n esc to interrupt", b = PROMPT_BORDER);
        assert_eq!(classify(&screen), Screen::Busy);
    }

    #[test]
    fn blank_or_wrong_console_is_unknown() {
        assert_eq!(classify(""), Screen::Unknown);
        assert_eq!(classify("\n\n  \n"), Screen::Unknown);
        assert_eq!(classify("PS C:\\> some shell prompt"), Screen::Unknown);
    }

    #[test]
    fn first_idle_read_latches_baseline() {
        // No baseline yet → start the streak and latch, never demote on read 1.
        assert_eq!(idle_step(0, None, SystemTime::UNIX_EPOCH), IdleStep::Latch);
    }

    #[test]
    fn transcript_advance_rearms() {
        // A write past the baseline (queued prompt while still working) re-arms
        // instead of demoting, even after the screen has looked idle.
        let base = SystemTime::UNIX_EPOCH;
        let later = base + Duration::from_secs(1);
        assert_eq!(idle_step(1, Some(base), later), IdleStep::Rearm);
    }

    #[test]
    fn quiet_idle_streak_demotes() {
        // Baseline unchanged across the streak → genuine cancel → demote once
        // the streak reaches IDLE_STREAK_TO_DEMOTE.
        let base = SystemTime::UNIX_EPOCH;
        assert_eq!(idle_step(1, Some(base), base), IdleStep::Demote);
    }

    fn queue_op(operation: &str) -> String {
        format!(r#"{{"type":"queue-operation","operation":"{operation}","sessionId":"s"}}"#)
    }

    #[test]
    fn outstanding_enqueue_is_pending() {
        // The user queued a prompt mid-turn and it hasn't been dequeued yet.
        let tail = [queue_op("enqueue")].join("\n");
        assert!(queued_pending(&tail));
    }

    #[test]
    fn enqueue_then_remove_is_not_pending() {
        // The queued prompt was dequeued to run → input box empty again.
        let tail = [queue_op("enqueue"), queue_op("remove")].join("\n");
        assert!(!queued_pending(&tail));
    }

    #[test]
    fn lone_remove_from_truncated_tail_is_not_pending() {
        // The matching enqueue scrolled above the tail window; a lone remove
        // means that prompt was consumed — net negative must read not-pending.
        let tail = [queue_op("remove")].join("\n");
        assert!(!queued_pending(&tail));
    }

    #[test]
    fn two_queued_one_consumed_is_still_pending() {
        let tail = [queue_op("enqueue"), queue_op("enqueue"), queue_op("remove")].join("\n");
        assert!(queued_pending(&tail));
    }

    #[test]
    fn no_queue_ops_is_not_pending() {
        let tail = [
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#.to_string(),
            "a prompt mentioning the words enqueue and queue-operation in prose".to_string(),
            "{ truncated leading line".to_string(),
        ]
        .join("\n");
        assert!(!queued_pending(&tail));
    }

    #[test]
    fn border_far_above_tail_is_unknown() {
        // The input box scrolled well above; only the tail counts.
        let mut screen = format!("{b}\n❯\n{b}\n", b = PROMPT_BORDER);
        for i in 0..TAIL_LINES {
            screen.push_str(&format!("output line {i}\n"));
        }
        screen.push_str("some-shell> ");
        assert_eq!(classify(&screen), Screen::Unknown);
    }
}
