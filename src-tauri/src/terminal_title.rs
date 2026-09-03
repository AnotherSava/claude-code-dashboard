//! Mirror each session's live status onto its terminal tab title as
//! "<status glyph> <name>" (e.g. "🔵 ai-dashboard").
//!
//! The dashboard is a GUI process with no handle into any terminal, so it
//! reaches the session's terminal through the pid candidates the hook
//! reports (`console_pids` on `/api/event`). On Windows we `AttachConsole`
//! to one of them and call `SetConsoleTitleW` — Windows Terminal / VS Code /
//! conhost all map the console title onto the tab, and the write needs no
//! window focus. On macOS we resolve a candidate's controlling tty
//! (`ps -o tty=`) and write an OSC 0 escape to the device — Terminal.app,
//! iTerm2, and kitty all map it onto the tab.
//!
//! The target pid comes from `session_registry`, which matches the row's cwd
//! against Claude Code's own list of live sessions — see that module for why
//! the process tree cannot answer this. The hook's ancestor chain
//! (`console_pids`, bounded at its own Claude Code process) remains only as the
//! fallback for a row the registry cannot place, and the two are never mixed:
//! once the registry names a pid it is the sole candidate, because falling
//! through to the chain after a failed write is exactly how one agent's status
//! reaches another's tab. No candidate at all is a legitimate outcome, not a
//! failure — a session with no terminal of its own has no tab to title.
//!
//! Everything is best-effort — a dead pid, a closed terminal, or a disabled
//! config flag degrade to "title doesn't change", never to an error the
//! caller sees.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

use crate::config::ConfigState;
use crate::notifications::context_percent;
use crate::state::{AgentSession, Status};

/// How long a pushed title is trusted to still be on the console. Spawned
/// console processes (bash.exe, pwsh.exe — every command the agent runs)
/// retitle the console on launch, and we have no notification when they do —
/// so a cached title older than this is re-pushed on the next sync. During
/// Working the transcript watcher emits constantly, so the title self-heals
/// within seconds; quiet states (blocked/done) spawn nothing that clobbers.
const REASSERT_MS: i64 = 5_000;

/// Managed state: which console pids belong to each chat_id, and the last
/// title pushed there with its timestamp (so repeated `sync` calls — every
/// `sessions_updated` emit — only touch the console when the title changed
/// or the push is old enough to have been clobbered).
#[derive(Default)]
pub struct TerminalTitles {
    pids: Mutex<HashMap<String, Vec<u32>>>,
    last: Mutex<HashMap<String, (String, i64)>>,
}

impl TerminalTitles {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the console-pid candidates a hook event reported for `chat_id`.
    /// The list mixes long-lived processes (Claude Code itself, the user's
    /// shell) with transient ones (the cmd.exe / python spawned per hook), and
    /// pid reuse could route a later write to the wrong terminal — so
    /// successive reports are intersected: after the second event only the
    /// pids present in both (the long-lived ones) survive. An empty
    /// intersection means the session moved to a different console (e.g. a
    /// restart in a new terminal); start over with the new list.
    pub fn register(&self, chat_id: &str, pids: &[u32]) {
        if pids.is_empty() {
            return;
        }
        let mut map = self.pids.lock().unwrap();
        let entry = map.entry(chat_id.to_string()).or_default();
        let merged: Vec<u32> = entry.iter().copied().filter(|p| pids.contains(p)).collect();
        *entry = if merged.is_empty() { pids.to_vec() } else { merged };
    }
}

/// A process can be attached to at most one console, so every
/// free→attach→…→free dance in [`with_console`] must hold this lock for its whole
/// duration or two threads would corrupt each other's console attachment.
#[cfg(windows)]
static ATTACH_LOCK: Mutex<()> = Mutex::new(());

// Declared by hand to avoid a `windows`/`windows-sys` dep, same as
// `auto_resize::nchittest` — these kernel32 signatures are ancient.
// `GetConsoleTitleW` returns the length in characters, 0 on failure: judge
// success by that length, never by `GetLastError`, which reads a stale value
// after a call that succeeded (203 `ERROR_ENVVAR_NOT_FOUND`, observed).
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn FreeConsole() -> i32;
    fn AttachConsole(pid: u32) -> i32;
    fn SetConsoleTitleW(title: *const u16) -> i32;
    fn GetConsoleTitleW(buf: *mut u16, size: u32) -> u32;
    fn GetConsoleWindow() -> isize;
}

#[cfg(windows)]
const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

/// Run `f` attached to the console of the first candidate that will have us, and
/// leave this process's own console attachment as it was found.
///
/// Shared by the writer ([`push_title`]) and the reader ([`read_title`]) because
/// the dance is the same and the restore step is the part a second copy would
/// drop: `FreeConsole` detaches the whole **process**, not the calling thread, so
/// forgetting to reattach costs a `cargo tauri dev` run its console output — a
/// failure that would show up nowhere near the copy that caused it.
///
/// Iteration stops at the first console successfully attached, not at the first
/// `Some`: attaching is what identifies the right console, and what `f` then makes
/// of it is `f`'s business.
#[cfg(windows)]
fn with_console<T>(candidates: impl IntoIterator<Item = u32>, f: impl Fn(u32) -> Option<T>) -> Option<T> {
    let _guard = ATTACH_LOCK.lock().unwrap();
    unsafe {
        let had_console = GetConsoleWindow() != 0;
        let mut out = None;
        for pid in candidates {
            FreeConsole();
            if AttachConsole(pid) != 0 {
                out = f(pid);
                break;
            }
        }
        FreeConsole();
        if had_console {
            // Dev runs (`cargo tauri dev`) start attached to the launching
            // terminal — reattach best-effort so console output keeps a home.
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        out
    }
}

/// The title currently on `pid`'s console, or `None` when it has none, refuses
/// the attach, or holds an empty title.
///
/// The read half of [`push_title`], and the Windows answer to "what is this
/// session showing" — deliberately per *session* rather than per window. On
/// Windows the title lives on the **console object**, one per tab, which is what
/// this dashboard writes and what every Windows terminal merely renders; so this
/// returns that session's own title whatever is on screen, where a terminal's
/// window title can only ever report its active tab. Verified against seven live
/// sessions sharing one window: seven distinct titles, order-independent.
#[cfg(windows)]
pub fn read_title(pid: u32) -> Option<String> {
    with_console([pid], |_| {
        let mut buf = [0u16; 512];
        let len = unsafe { GetConsoleTitleW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
    })
}

fn status_glyph(status: Status) -> &'static str {
    // Mirrors the status pill colors in SessionItem.svelte.
    match status {
        Status::Idle => "⚪",
        Status::Working => "🔵",
        // No light-blue *circle* emoji exists to mirror the dashboard pill, so
        // `Waiting` (main turn settled, background work still running) uses an
        // hourglass — its "still going, hang on" semantics separate it cleanly
        // from `Working`'s blue circle.
        Status::Waiting => "⏳",
        // Not a circle: the orange and red circles read too alike in a tab, so
        // `Blocked` (waiting on the user) uses a raised hand — its "stop, your
        // turn" semantics also separate it cleanly from `Error`'s red circle.
        Status::Blocked => "✋",
        Status::Done => "🟢",
        Status::Error => "🔴",
    }
}

/// One reading of a tab title *this* dashboard wrote.
pub struct TitleReading<'a> {
    /// The status its glyph names.
    pub status: Status,
    /// Everything after the glyph, suffixes included — deliberately not the bare
    /// name. `build_title` appends " [N%]" and " ⚠" today and will grow more, so
    /// a parser that stripped them would have to learn each one; [`names`] does
    /// prefix matching instead.
    ///
    /// [`names`]: TitleReading::names
    rest: &'a str,
}

impl TitleReading<'_> {
    /// Whether this title is the one written for a row labelled `label`.
    pub fn names(&self, label: &str) -> bool {
        self.rest == label || self.rest.starts_with(&format!("{label} "))
    }
}

/// Whether two tab titles this dashboard wrote name the same row.
///
/// The question a terminal that has no session handle of its own has to ask.
/// agterm keys its selection on agterm's `sessionID`, so it compares two ids and
/// is immune to anything the title does; Windows Terminal publishes only the
/// active tab's *title*, so the Windows adapter's only key is this — and a title
/// changes for two quite different reasons. The user switched tabs, or **we**
/// rewrote the tab we are already on: a glyph moving `🟢 x` → `🔵 x`, the
/// context suffix ticking `[76%]` → `[77%]`, a drift badge appearing. Calling one
/// of ours a switch marks the row read on our own write, which hides finished
/// work — the one direction this must never fail in.
///
/// It answers by looking for a label both titles would answer [`names`] for,
/// which is why it needs no list of suffixes and cannot rot as `build_title`
/// grows one. `rest`'s doc comment refuses a parser that strips known suffixes,
/// and this is not one: the only structure assumed is the one `names` already
/// assumes, that a title is a label followed by space-separated extras.
///
/// **It errs toward "the same row".** Two rows whose labels share a first word
/// (`my proj`, `my other`) compare equal, so switching between them reports
/// nothing. That is a missed observation, which leaves the row *showing* — the
/// recoverable direction — where the opposite error hides it. A title we did not
/// write names no row at all, so it is never the same row as anything, including
/// another title we did not write.
///
/// [`names`]: TitleReading::names
pub fn same_row(a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (parse_title(a), parse_title(b)) else { return false };
    // Every token-boundary prefix of one title's rest, longest first, asked of
    // the other. `a.names(label)` holds by construction for each candidate, so
    // the test that matters is `b`'s.
    std::iter::once(a.rest).chain(a.rest.rmatch_indices(' ').map(|(i, _)| &a.rest[..i])).any(|label| !label.is_empty() && b.names(label))
}

/// Read back a title this dashboard wrote, or `None` for anything else — a
/// shell's own `~/proj — zsh`, a tab we have never titled, a blank one.
///
/// It lives beside [`status_glyph`] and [`build_title`] rather than in either
/// reader, because the two halves of one map must not be able to drift: the
/// round-trip test below is what keeps the glyphs distinct when a seventh status
/// is added. Two readers depend on it — `attention::resolve_row`, which wants
/// only the name, and `session_restore`, which wants the status a tab has been
/// holding for us across a restart.
///
/// An unrecognized leading token yields `None` rather than a guess: a title we
/// did not write says nothing about a row, and inventing a status from one would
/// be the single worst thing this parser could do.
pub fn parse_title(title: &str) -> Option<TitleReading<'_>> {
    let (head, rest) = title.trim().split_once(' ')?;
    Some(TitleReading { status: status_from_glyph(head)?, rest: rest.trim() })
}

/// The inverse of [`status_glyph`]. Total over the glyphs that function emits
/// and `None` everywhere else.
fn status_from_glyph(glyph: &str) -> Option<Status> {
    Some(match glyph {
        "⚪" => Status::Idle,
        "🔵" => Status::Working,
        "⏳" => Status::Waiting,
        "✋" => Status::Blocked,
        "🟢" => Status::Done,
        "🔴" => Status::Error,
        _ => return None,
    })
}

/// The tab title for a session: "<glyph> <name>", with " [N%]" appended when
/// the session's context usage is at least `context_threshold` percent of its
/// model's window (the same figure as the token counter), and a trailing " ⚠"
/// when the instruction-adherence canary has flagged the row. `context_threshold
/// <= 0` — or an unknown percentage (no tokens / model / window) — omits the
/// percent suffix. The drift warning is orthogonal to status, so it rides
/// alongside whatever glyph the state resolves to. Pure and testable; the
/// console-write side effects live in `push_title`.
fn build_title(session: &AgentSession, context_threshold: f32, window_tokens: &HashMap<String, u64>) -> String {
    let name = session.display_name.as_deref().unwrap_or(&session.id);
    let mut title = format!("{} {}", status_glyph(session.status), name);
    if context_threshold > 0.0 {
        if let Some(pct) = context_percent(session, window_tokens) {
            if pct >= context_threshold {
                let _ = write!(title, " [{}%]", pct.round() as u32);
            }
        }
    }
    if session.instruction_drift {
        let _ = write!(title, " ⚠");
    }
    title
}

/// Reconcile terminal tab titles with the current sessions. Called from
/// `emit_sessions_updated`, which every state transition already flows
/// through (hook events, transcript watcher, renames, row removal) — so the
/// tab tracks everything the row shows, with no second state machine.
/// Sessions that vanished (SessionEnd, row removed) get a blank title — the
/// terminal falls back to its default — and are forgotten.
pub fn sync(app: &AppHandle, sessions: &[AgentSession]) {
    let Some(titles) = app.try_state::<TerminalTitles>() else {
        return;
    };
    let cfg = app.try_state::<ConfigState>().map(|s| s.snapshot());
    let enabled = cfg.as_ref().map(|c| c.terminal_titles).unwrap_or(true);
    let mut pids = titles.pids.lock().unwrap();
    let mut last = titles.last.lock().unwrap();
    let now = crate::commands::now_ms();

    // Claude Code's own session registry answers "which process owns this row's
    // terminal" directly; the hook's ancestor chain only answers "what is above
    // the hook", which diverges the moment a conversation runs somewhere other
    // than its tab. Prefer the registry, and when it has an answer use *only*
    // it — falling through to the chain on a failed write is how a status
    // reaches a neighbour's tab. No answer (an older Claude Code, an
    // unreadable registry, an ambiguous cwd) keeps the chain as it was.
    let registry = app.try_state::<crate::session_registry::SessionRegistry>();
    let root = cfg.as_ref().and_then(|c| c.projects_root.clone());
    let resolve = |chat_id: &str, chain: &[u32]| -> Vec<u32> {
        match registry.as_ref().and_then(|r| r.tab_pid(chat_id, root.as_deref(), now)) {
            Some(pid) => vec![pid],
            None => chain.to_vec(),
        }
    };

    // Blanking a tab is driven by `last` — every chat_id this process has
    // titled — and deliberately not by `pids`, which a hook event populates
    // (`titles.register`) while the *write target* comes from the session
    // registry. So a row the registry placed and no hook ever reported is absent
    // from `pids` and present in `last`, and sweeping `pids` left our own last
    // glyph sitting on a tab whose row had gone — which is exactly the stale
    // title `session_restore` reads back on the next start.
    let blank = |chat_id: &str, last: &mut HashMap<String, (String, i64)>| {
        last.remove(chat_id);
        push_title(&resolve(chat_id, pids.get(chat_id).map_or(&[][..], Vec::as_slice)), "");
    };

    if !enabled {
        // Toggled off: blank every title we have written, keep the pid map so
        // re-enabling resumes without waiting for the next hook event.
        for chat_id in last.keys().cloned().collect::<Vec<_>>() {
            blank(&chat_id, &mut last);
        }
        return;
    }

    let live: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    for chat_id in last.keys().filter(|id| !live.contains(id.as_str())).cloned().collect::<Vec<_>>() {
        blank(&chat_id, &mut last);
    }
    pids.retain(|chat_id, _| live.contains(chat_id.as_str()));

    let context_threshold = cfg.as_ref().and_then(|c| c.terminal_title_context_percent).unwrap_or(0.0);
    let empty_tokens = HashMap::new();
    let window_tokens = cfg.as_ref().map(|c| &c.context_window_tokens).unwrap_or(&empty_tokens);

    for s in sessions {
        let candidates = resolve(&s.id, pids.get(&s.id).map_or(&[][..], Vec::as_slice));
        if candidates.is_empty() {
            continue;
        }
        let title = build_title(s, context_threshold, window_tokens);
        if let Some((prev, at)) = last.get(&s.id) {
            if *prev == title && now - at < REASSERT_MS {
                continue;
            }
        }
        if push_title(&candidates, &title) {
            last.insert(s.id.clone(), (title, now));
        }
    }
}

/// Set the console title of the first reachable candidate pid. Returns true
/// when a title was actually written — a false return leaves the `last` cache
/// untouched so the next sync retries.
#[cfg(windows)]
fn push_title(candidates: &[u32], title: &str) -> bool {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // Far-to-near: the hook reports candidates ordered nearest-first (its own
    // console processes, then parent, grandparent, …). The near end is transient
    // per-hook processes holding a fresh *invisible* console (hooks are spawned
    // CREATE_NO_WINDOW) — a title written there is lost. The far end is GUI
    // ancestors (Windows Terminal, explorer) where attach simply fails. So
    // walking from the far end, the first successful attach is the user's shell
    // or Claude Code itself — the real terminal console. (GetConsoleWindow can't
    // discriminate instead: conPTY consoles report no window on current Windows
    // 11, same as invisible ones.)
    with_console(candidates.iter().rev().copied(), |pid| {
        let ok = unsafe { SetConsoleTitleW(wide.as_ptr()) } != 0;
        tracing::debug!(pid, ok, title, "terminal title written");
        Some(ok)
    })
    .unwrap_or(false)
}

/// macOS/Linux: resolve the candidate's controlling tty via `ps -o tty=` and
/// write an OSC 0 (icon + window title) escape to the device. Near-to-far,
/// unlike Windows: there is no attach dance whose first success must be the
/// real console — transient per-hook pids are usually dead by now (`ps`
/// prints nothing) and Claude Code detaches every child it spawns from the
/// controlling terminal, so the hook-side pids report `??` and fall through
/// to Claude Code itself, which holds the tty of the visible tab. Skipping
/// past `??` is only safe because the chain stops at that process: an
/// all-`??` chain means this session owns no terminal, and returning false
/// leaves the title unwritten rather than climbing into someone else's tab.
#[cfg(not(windows))]
fn push_title(candidates: &[u32], title: &str) -> bool {
    use std::io::Write;
    for &pid in candidates {
        let Ok(out) = std::process::Command::new("ps").args(["-o", "tty=", "-p", &pid.to_string()]).output() else { continue };
        let tty_raw = String::from_utf8_lossy(&out.stdout);
        let tty = tty_raw.trim();
        if tty.is_empty() || tty.starts_with('?') {
            continue;
        }
        let Ok(mut dev) = std::fs::OpenOptions::new().write(true).open(format!("/dev/{tty}")) else { continue };
        if dev.write_all(format!("\x1b]0;{title}\x07").as_bytes()).is_ok() {
            tracing::debug!(pid, tty, title, "terminal title written");
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_glyph_covers_every_status() {
        assert_eq!(status_glyph(Status::Working), "🔵");
        // Waiting must stay distinct from Working — not the shared blue circle.
        assert_eq!(status_glyph(Status::Waiting), "⏳");
        assert_ne!(status_glyph(Status::Waiting), status_glyph(Status::Working));
        // Blocked must stay distinct from Error — the orange/red circles read
        // too alike, so Blocked is a raised hand, not a circle.
        assert_eq!(status_glyph(Status::Blocked), "✋");
        assert_ne!(status_glyph(Status::Blocked), status_glyph(Status::Error));
        assert_eq!(status_glyph(Status::Done), "🟢");
        assert_eq!(status_glyph(Status::Error), "🔴");
        assert_eq!(status_glyph(Status::Idle), "⚪");
    }

    /// The two halves of the map must not drift. `session_restore` reads a status
    /// back out of a tab title, so a seventh status sharing a glyph would make one
    /// of them silently unrecoverable — this is what catches that at the moment it
    /// is added rather than after a restart shows the wrong pill.
    /// Every `Status`, listed through an exhaustive `match` so adding a seventh
    /// variant fails to *compile* here rather than silently skipping every row in
    /// that state after the next restart. A hard-coded array would have passed.
    const ALL_STATUSES: [Status; 6] = [Status::Idle, Status::Working, Status::Waiting, Status::Blocked, Status::Done, Status::Error];

    #[test]
    fn the_status_list_these_tests_use_is_every_status() {
        for status in ALL_STATUSES {
            // No wildcard arm: a new variant breaks the build right here.
            match status {
                Status::Idle | Status::Working | Status::Waiting | Status::Blocked | Status::Done | Status::Error => {}
            }
        }
        assert_eq!(ALL_STATUSES.iter().map(|s| status_glyph(*s)).collect::<std::collections::HashSet<_>>().len(), ALL_STATUSES.len(), "every glyph is distinct, so `parse_title` can invert the map");
    }

    #[test]
    fn every_glyph_round_trips_back_to_its_own_status() {
        for status in ALL_STATUSES {
            let title = format!("{} dash", status_glyph(status));
            let reading = parse_title(&title).expect("parses");
            assert_eq!(reading.status, status, "{status:?}");
            assert!(reading.names("dash"));
        }
    }

    #[test]
    fn a_title_this_dashboard_did_not_write_yields_nothing() {
        // Inventing a status from a stranger's title is the worst thing this
        // parser could do: `session_restore` would create a row asserting it.
        for title in ["", "   ", "dash", "~/Projects/dash — zsh", "• dash", "🟡 dash"] {
            assert!(parse_title(title).is_none(), "{title:?}");
        }
    }

    #[test]
    fn a_reading_names_its_row_through_every_suffix_build_title_appends() {
        // Matching the whole string would have to learn each suffix separately.
        for title in ["✋ dash", "✋ dash [62%]", "✋ dash ⚠", "✋ dash [62%] ⚠"] {
            let reading = parse_title(title).expect("parses");
            assert_eq!(reading.status, Status::Blocked);
            assert!(reading.names("dash"), "{title}");
            assert!(!reading.names("das"), "a prefix of the name is not the name: {title}");
            assert!(!reading.names("dashboard"), "{title}");
        }
    }

    #[test]
    fn every_suffix_build_title_can_append_still_names_the_same_row() {
        // The property the Windows adapter's whole departure signal rests on, and
        // the reason `same_row` looks for a shared label rather than stripping
        // known suffixes: this must keep holding when `build_title` grows a
        // seventh one, without anything here being updated.
        let mut s = session("what-is-next", Some("m"), Some(140_000));
        s.status = Status::Done;
        let plain = build_title(&s, 0.0, &tokens_map());
        let with_context = build_title(&s, 50.0, &tokens_map());
        s.instruction_drift = true;
        let with_both = build_title(&s, 50.0, &tokens_map());
        s.status = Status::Working;
        let other_glyph = build_title(&s, 50.0, &tokens_map());
        for (a, b) in [(&plain, &with_context), (&plain, &with_both), (&with_context, &with_both), (&with_both, &other_glyph)] {
            assert!(same_row(a, b), "{a} / {b}");
            assert!(same_row(b, a), "and the other way round: {b} / {a}");
        }
    }

    #[test]
    fn our_own_rewrite_of_one_tab_is_never_a_switch() {
        // Both measured live: a glyph moving when the user prompts the session
        // already on screen, and the context suffix ticking while it works.
        assert!(same_row("🟢 bga-assistant", "🔵 bga-assistant"));
        assert!(same_row("🟢 what-is-next [76%]", "🟢 what-is-next [77%]"));
    }

    #[test]
    fn two_different_rows_are_not_the_same_row() {
        assert!(!same_row("🟢 transcripts", "🟢 what-is-next"));
        // A name that is a prefix of another is the trap `names` already guards.
        assert!(!same_row("🟢 dash", "🟢 dashboard"));
        assert!(!same_row("🟢 dashboard [10%]", "🟢 dash [10%]"));
    }

    #[test]
    fn a_title_we_did_not_write_is_never_the_same_row_as_anything() {
        // Which is what makes leaving a tracked tab for a plain shell a
        // departure, and what stops two foreign titles being confused for one row.
        assert!(!same_row("🟢 transcripts", "powershell"));
        assert!(!same_row("powershell", "🟢 transcripts"));
        assert!(!same_row("powershell", "pwsh"));
        assert!(!same_row("", ""));
    }

    #[test]
    fn labels_sharing_a_first_word_are_treated_as_one_row() {
        // The documented direction of error. It costs a missed observation, which
        // leaves the row *showing*; the opposite error would hide finished work.
        assert!(same_row("🟢 my proj", "🟢 my other"));
    }

    #[test]
    fn a_built_title_round_trips_through_the_parser() {
        // The end-to-end property `session_restore` depends on, exercised against
        // `build_title` itself rather than a hand-written string.
        let mut s = session("what-is-next", None, None);
        s.status = Status::Done;
        let title = build_title(&s, 0.0, &HashMap::new());
        let reading = parse_title(&title).expect("parses");
        assert_eq!(reading.status, Status::Done);
        assert!(reading.names("what-is-next"));
    }

    fn candidates(t: &TerminalTitles, id: &str) -> Vec<u32> {
        t.pids.lock().unwrap().get(id).cloned().unwrap_or_default()
    }

    fn session(id: &str, model: Option<&str>, tokens: Option<u64>) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "test".to_string(),
            model: model.map(String::from),
            input_tokens: tokens,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: None,
            instruction_drift: false,
            canary: crate::state::Canary::Off,
            attended_at: None,
            name_shared_by: None,
        }
    }

    fn tokens_map() -> HashMap<String, u64> {
        [("m".to_string(), 200_000u64)].into_iter().collect()
    }

    #[test]
    fn build_title_appends_context_at_or_above_threshold() {
        let w = tokens_map();
        // 100k / 200k = 50%, exactly at the default threshold → suffix appears.
        assert_eq!(build_title(&session("proj", Some("m"), Some(100_000)), 50.0, &w), "🔵 proj [50%]");
        // 134k / 200k = 67% → rounded suffix.
        assert_eq!(build_title(&session("proj", Some("m"), Some(134_000)), 50.0, &w), "🔵 proj [67%]");
    }

    #[test]
    fn build_title_omits_context_below_threshold() {
        let w = tokens_map();
        // 98k / 200k = 49% → below 50, no suffix.
        assert_eq!(build_title(&session("proj", Some("m"), Some(98_000)), 50.0, &w), "🔵 proj");
    }

    #[test]
    fn build_title_threshold_zero_disables_suffix() {
        let w = tokens_map();
        // Even a full window shows no suffix when the feature is off (0/null).
        assert_eq!(build_title(&session("proj", Some("m"), Some(200_000)), 0.0, &w), "🔵 proj");
    }

    #[test]
    fn build_title_omits_context_when_uncomputable() {
        let w = tokens_map();
        // No tokens, or a model with no configured window → no percentage known.
        assert_eq!(build_title(&session("proj", Some("m"), None), 50.0, &w), "🔵 proj");
        assert_eq!(build_title(&session("proj", Some("other"), Some(180_000)), 50.0, &w), "🔵 proj");
        assert_eq!(build_title(&session("proj", None, Some(180_000)), 50.0, &w), "🔵 proj");
    }

    #[test]
    fn build_title_appends_drift_warning_alongside_status_and_context() {
        let w = tokens_map();
        let mut s = session("proj", Some("m"), Some(100_000));
        s.instruction_drift = true;
        // The ⚠ rides after the context suffix, and the status glyph is untouched.
        assert_eq!(build_title(&s, 50.0, &w), "🔵 proj [50%] ⚠");
        // …and shows with no context suffix too.
        assert_eq!(build_title(&session("proj", Some("m"), Some(100_000)), 50.0, &w), "🔵 proj [50%]");
        s.input_tokens = None;
        assert_eq!(build_title(&s, 50.0, &w), "🔵 proj ⚠");
    }

    #[test]
    fn build_title_uses_display_name_and_current_glyph() {
        let w = tokens_map();
        let mut s = session("proj", Some("m"), Some(180_000));
        s.display_name = Some("printlab".into());
        s.status = Status::Blocked;
        assert_eq!(build_title(&s, 50.0, &w), "✋ printlab [90%]");
    }

    #[test]
    fn register_first_report_is_taken_verbatim() {
        let t = TerminalTitles::new();
        t.register("a", &[100, 200, 300]);
        assert_eq!(candidates(&t, "a"), vec![100, 200, 300]);
    }

    #[test]
    fn register_intersects_so_transient_pids_drop_out() {
        let t = TerminalTitles::new();
        // 100 = claude, 200 = shell, 300/301 = per-hook cmd.exe.
        t.register("a", &[100, 200, 300]);
        t.register("a", &[100, 200, 301]);
        assert_eq!(candidates(&t, "a"), vec![100, 200]);
    }

    #[test]
    fn register_disjoint_report_replaces_stale_console() {
        let t = TerminalTitles::new();
        t.register("a", &[100, 200]);
        // Session restarted in a different terminal: nothing overlaps.
        t.register("a", &[500, 600]);
        assert_eq!(candidates(&t, "a"), vec![500, 600]);
    }

    #[test]
    fn register_empty_report_keeps_existing_candidates() {
        let t = TerminalTitles::new();
        t.register("a", &[100]);
        t.register("a", &[]);
        assert_eq!(candidates(&t, "a"), vec![100]);
    }
}
