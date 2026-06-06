//! Mirror each session's live status onto its terminal tab title as
//! "<colored circle> <name>" (e.g. "🔵 ai-dashboard").
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
//! Everything is best-effort — a dead pid, a closed terminal, or a disabled
//! config flag degrade to "title doesn't change", never to an error the
//! caller sees.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

use crate::config::ConfigState;
use crate::state::{AgentSession, Status};

/// How long a pushed title is trusted to still be on the console. Spawned
/// console processes (bash.exe, pwsh.exe — every command the agent runs)
/// retitle the console on launch, and we have no notification when they do —
/// so a cached title older than this is re-pushed on the next sync. During
/// Working the transcript watcher emits constantly, so the title self-heals
/// within seconds; quiet states (awaiting/done) spawn nothing that clobbers.
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

    /// The console-pid candidates currently tracked for `chat_id` (intersected
    /// down to the long-lived processes). Empty if none reported yet. Used by
    /// `idle_probe` to read the same console the title is written to.
    pub fn candidates(&self, chat_id: &str) -> Vec<u32> {
        self.pids.lock().unwrap().get(chat_id).cloned().unwrap_or_default()
    }
}

/// A process can be attached to at most one console, so every
/// free→attach→…→free dance — title writes (`push_title`) and screen reads
/// (`read_console_screen`) alike — must hold this lock for its whole duration
/// or two threads would corrupt each other's console attachment.
#[cfg(windows)]
static ATTACH_LOCK: Mutex<()> = Mutex::new(());

fn circle(status: Status) -> &'static str {
    // Mirrors the status pill colors in SessionItem.svelte.
    match status {
        Status::Idle => "⚪",
        Status::Working => "🔵",
        Status::Awaiting => "🟠",
        Status::Done => "🟢",
        Status::Error => "🔴",
    }
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
    let enabled = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().terminal_titles)
        .unwrap_or(true);
    let mut pids = titles.pids.lock().unwrap();
    let mut last = titles.last.lock().unwrap();

    if !enabled {
        // Toggled off: blank every title we have written, keep the pid map so
        // re-enabling resumes without waiting for the next hook event.
        for (chat_id, candidates) in pids.iter() {
            if last.remove(chat_id).is_some() {
                push_title(candidates, "");
            }
        }
        return;
    }

    let live: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    pids.retain(|chat_id, candidates| {
        if live.contains(chat_id.as_str()) {
            return true;
        }
        if last.remove(chat_id).is_some() {
            push_title(candidates, "");
        }
        false
    });

    let now = crate::commands::now_ms();
    for s in sessions {
        let Some(candidates) = pids.get(&s.id) else {
            continue;
        };
        let name = s.display_name.as_deref().unwrap_or(&s.id);
        let title = format!("{} {}", circle(s.status), name);
        if let Some((prev, at)) = last.get(&s.id) {
            if *prev == title && now - at < REASSERT_MS {
                continue;
            }
        }
        if push_title(candidates, &title) {
            last.insert(s.id.clone(), (title, now));
        }
    }
}

/// Set the console title of the first reachable candidate pid. Returns true
/// when a title was actually written — a false return leaves the `last` cache
/// untouched so the next sync retries.
#[cfg(windows)]
fn push_title(candidates: &[u32], title: &str) -> bool {
    // Declared by hand to avoid a `windows`/`windows-sys` dep, same as
    // `auto_resize::nchittest` — these kernel32 signatures are ancient.
    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(pid: u32) -> i32;
        fn SetConsoleTitleW(title: *const u16) -> i32;
        fn GetConsoleWindow() -> isize;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

    let _guard = ATTACH_LOCK.lock().unwrap();
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let had_console = GetConsoleWindow() != 0;
        let mut ok = false;
        // Far-to-near: the hook reports candidates ordered nearest-first
        // (its own console processes, then parent, grandparent, …). The near
        // end is transient per-hook processes holding a fresh *invisible*
        // console (hooks are spawned CREATE_NO_WINDOW) — a title written
        // there is lost. The far end is GUI ancestors (Windows Terminal,
        // explorer) where attach simply fails. So walking from the far end,
        // the first successful attach is the user's shell or Claude Code
        // itself — the real terminal console. (GetConsoleWindow can't
        // discriminate instead: conPTY consoles report no window on current
        // Windows 11, same as invisible ones.)
        for &pid in candidates.iter().rev() {
            FreeConsole();
            if AttachConsole(pid) != 0 {
                ok = SetConsoleTitleW(wide.as_ptr()) != 0;
                tracing::debug!(pid, ok, title, "terminal title written");
                break;
            }
        }
        FreeConsole();
        if had_console {
            // Dev runs (`cargo tauri dev`) start attached to the launching
            // terminal — reattach best-effort so console output keeps a home.
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        ok
    }
}

/// Read the visible screen of the first reachable candidate's console as
/// plaintext (one string per visible row, newline-joined), or `None` if no
/// candidate could be attached and read. Same far-to-near attach rationale as
/// `push_title`: the first successful attach is the user's real terminal, so
/// we read that one and stop. Shares `ATTACH_LOCK` with `push_title` — both
/// juggle this process's single console attachment.
#[cfg(windows)]
pub fn read_console_screen(candidates: &[u32]) -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(pid: u32) -> i32;
        fn GetConsoleWindow() -> isize;
        fn CreateFileW(name: *const u16, access: u32, share: u32, sec: *const core::ffi::c_void, disposition: u32, flags: u32, template: isize) -> isize;
        fn CloseHandle(h: isize) -> i32;
        fn GetConsoleScreenBufferInfo(h: isize, info: *mut Csbi) -> i32;
        fn ReadConsoleOutputCharacterW(h: isize, buf: *mut u16, len: u32, read_coord: u32, chars_read: *mut u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const GENERIC_READ_WRITE: u32 = 0xC000_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE: isize = -1;

    // CONSOLE_SCREEN_BUFFER_INFO, field-for-field: two COORDs, a WORD, a
    // SMALL_RECT, a COORD. `srWindow` is the visible viewport into a possibly
    // taller scrollback buffer; we read only it.
    #[repr(C)]
    #[derive(Default)]
    struct Csbi {
        size_x: i16,
        size_y: i16,
        cursor_x: i16,
        cursor_y: i16,
        attributes: u16,
        win_left: i16,
        win_top: i16,
        win_right: i16,
        win_bottom: i16,
        max_x: i16,
        max_y: i16,
    }

    let _guard = ATTACH_LOCK.lock().unwrap();
    let conout: Vec<u16> = "CONOUT$".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let had_console = GetConsoleWindow() != 0;
        let mut result = None;
        for &pid in candidates.iter().rev() {
            FreeConsole();
            if AttachConsole(pid) == 0 {
                continue;
            }
            // First attach succeeds on the real terminal; read it and stop
            // (nearer consoles are invisible per-hook ones — no point reading).
            let h = CreateFileW(conout.as_ptr(), GENERIC_READ_WRITE, FILE_SHARE_READ_WRITE, std::ptr::null(), OPEN_EXISTING, 0, 0);
            if h != INVALID_HANDLE && h != 0 {
                let mut info = Csbi::default();
                if GetConsoleScreenBufferInfo(h, &mut info) != 0 && info.size_x > 0 {
                    let width = info.size_x as u32;
                    let mut lines: Vec<String> = Vec::new();
                    let mut buf = vec![0u16; width as usize];
                    for y in info.win_top..=info.win_bottom {
                        let coord = ((y as u32) << 16) | (info.win_left as u32 & 0xFFFF);
                        let mut read = 0u32;
                        if ReadConsoleOutputCharacterW(h, buf.as_mut_ptr(), width, coord, &mut read) != 0 {
                            lines.push(String::from_utf16_lossy(&buf[..read as usize]).trim_end().to_string());
                        }
                    }
                    result = Some(lines.join("\n"));
                }
                CloseHandle(h);
            }
            break;
        }
        FreeConsole();
        if had_console {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        result
    }
}

#[cfg(not(windows))]
pub fn read_console_screen(_candidates: &[u32]) -> Option<String> {
    None
}

/// macOS/Linux: resolve the candidate's controlling tty via `ps -o tty=` and
/// write an OSC 0 (icon + window title) escape to the device. Near-to-far,
/// unlike Windows: there is no attach dance whose first success must be the
/// real console — transient per-hook pids are usually dead by now (`ps`
/// prints nothing) and GUI ancestors (the terminal emulator itself) report
/// `??`, so both fall through to the long-lived Claude Code / shell pids,
/// which share the controlling tty of the visible tab.
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
    fn circle_covers_every_status() {
        assert_eq!(circle(Status::Working), "🔵");
        assert_eq!(circle(Status::Awaiting), "🟠");
        assert_eq!(circle(Status::Done), "🟢");
        assert_eq!(circle(Status::Error), "🔴");
        assert_eq!(circle(Status::Idle), "⚪");
    }

    fn candidates(t: &TerminalTitles, id: &str) -> Vec<u32> {
        t.pids.lock().unwrap().get(id).cloned().unwrap_or_default()
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
