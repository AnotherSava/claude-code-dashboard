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
//! Windows-only: reading a console's screen buffer is a Win32 facility with no
//! macOS equivalent. [`crate::terminal_title::read_console_screen`] returns
//! `None` off-Windows, so the loop never demotes there.

use std::collections::HashMap;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::{emit_sessions_updated, now_ms};
use crate::config::ConfigState;
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

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // chat_id → consecutive idle reads.
        let mut idle_streak: HashMap<String, u32> = HashMap::new();
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("idle probe started");

        loop {
            ticker.tick().await;

            let Some(cfg_state) = app.try_state::<ConfigState>() else { continue };
            if !cfg_state.snapshot().detect_cancelled_turns {
                idle_streak.clear();
                continue;
            }
            let Some(app_state) = app.try_state::<AppState>() else { continue };
            let Some(titles) = app.try_state::<TerminalTitles>() else { continue };

            let sessions = app_state.snapshot();
            // Drop bookkeeping for any session no longer Working.
            idle_streak.retain(|id, _| {
                sessions.iter().any(|s| s.id == *id && s.status == Status::Working)
            });

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
                        let streak = idle_streak.entry(s.id.clone()).or_insert(0);
                        *streak += 1;
                        if *streak >= IDLE_STREAK_TO_DEMOTE
                            && app_state.demote_working_to_idle(&s.id, now_ms())
                        {
                            tracing::debug!(id = %s.id, "idle prompt with no in-flight turn; demoted to idle");
                            idle_streak.remove(&s.id);
                            emit_sessions_updated(&app);
                        }
                    }
                    // Busy or an unrecognised screen both reset the streak —
                    // only an unbroken run of positive idle reads demotes.
                    Screen::Busy | Screen::Unknown => {
                        idle_streak.remove(&s.id);
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
