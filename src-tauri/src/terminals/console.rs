//! The Windows console object: reading and writing the title it holds.
//!
//! On Windows a session's title lives on its **console**, one per tab, and every
//! terminal there (Windows Terminal, VS Code's terminal, a bare conhost) merely
//! renders it, so one mechanism serves all of them. The Windows adapter
//! answers [`TerminalAdapter::write_title`](super::TerminalAdapter::write_title)
//! and [`TerminalAdapter::sessions`](super::TerminalAdapter::sessions) through
//! this module; nothing outside `terminals/` calls it.

use std::sync::Mutex;

use super::{TitleLanding, TitleWrite};

/// A process can be attached to at most one console, so every
/// free→attach→…→free dance in [`with_console`] must hold this lock for its whole
/// duration or two threads would corrupt each other's console attachment.
static ATTACH_LOCK: Mutex<()> = Mutex::new(());

// Declared by hand to avoid a `windows`/`windows-sys` dep, same as
// `auto_resize::nchittest` — these kernel32 signatures are ancient.
// `GetConsoleTitleW` returns the length in characters, 0 on failure: judge
// success by that length, never by `GetLastError`, which reads a stale value
// after a call that succeeded (203 `ERROR_ENVVAR_NOT_FOUND`, observed).
#[link(name = "kernel32")]
extern "system" {
    fn FreeConsole() -> i32;
    fn AttachConsole(pid: u32) -> i32;
    fn SetConsoleTitleW(title: *const u16) -> i32;
    fn GetConsoleTitleW(buf: *mut u16, size: u32) -> u32;
    fn GetConsoleWindow() -> isize;
    fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
}

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

/// Run `f` attached to the console of the first candidate that will have us, and
/// leave this process's own console attachment as it was found.
///
/// Shared by the writer ([`write_title`]) and the reader ([`read_title`]) because
/// the dance is the same and the restore step is the part a second copy would
/// drop: `FreeConsole` detaches the whole **process**, not the calling thread, so
/// forgetting to reattach costs a `cargo tauri dev` run its console output — a
/// failure that would show up nowhere near the copy that caused it.
///
/// Iteration stops at the first console successfully attached, not at the first
/// `Some`: attaching is what identifies the right console, and what `f` then makes
/// of it is `f`'s business.
///
/// `Err` when no candidate attached, carrying each refusal, so a caller can say
/// why rather than fail silently: a refused attach is otherwise indistinguishable
/// from a write that was never attempted.
fn with_console<T>(candidates: impl IntoIterator<Item = u32>, f: impl Fn(u32) -> Option<T>) -> Result<Option<T>, Vec<AttachRefusal>> {
    let _guard = ATTACH_LOCK.lock().unwrap();
    unsafe {
        let had_console = GetConsoleWindow() != 0;
        let mut out = Err(Vec::new());
        for pid in candidates {
            FreeConsole();
            if AttachConsole(pid) != 0 {
                out = Ok(f(pid));
                break;
            }
            // Read before any other call can overwrite it. Unlike the
            // `GetConsoleTitleW` note above, this follows a call that *failed*,
            // which is when `AttachConsole` is documented to set it.
            let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if let Err(refusals) = &mut out {
                refusals.push(AttachRefusal { pid, error });
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

/// A candidate whose console [`with_console`] could not attach to, with the
/// Win32 error `AttachConsole` set.
#[derive(Debug)]
struct AttachRefusal {
    pid: u32,
    error: i32,
}

/// The title currently on `pid`'s console, or `None` when it has none, refuses
/// the attach, or holds an empty title.
///
/// The read half of [`write_title`], and the Windows answer to "what is this
/// session showing" — deliberately per *session* rather than per window. On
/// Windows the title lives on the **console object**, one per tab, which is what
/// this dashboard writes and what every Windows terminal merely renders; so this
/// returns that session's own title whatever is on screen, where a terminal's
/// window title can only ever report its active tab. Verified against seven live
/// sessions sharing one window: seven distinct titles, order-independent.
pub fn read_title(pid: u32) -> Option<String> {
    with_console([pid], |_| {
        let mut buf = [0u16; 512];
        // Clamped, and not defensively: `GetConsoleTitleW` is documented to
        // return the length of the *console's* title rather than the number of
        // characters it copied, so a title longer than the buffer yields an index
        // past its end and `&buf[..len]` panics — inside `with_console`, holding
        // `ATTACH_LOCK`, in an always-on-top widget. Truncation is the right
        // outcome anyway: a truncated title is not one we wrote, so it names no
        // row, which is the safe direction.
        let len = (unsafe { GetConsoleTitleW(buf.as_mut_ptr(), buf.len() as u32) } as usize).min(buf.len());
        (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
    })
    .unwrap_or_else(|refusals| {
        for r in refusals {
            tracing::debug!(pid = r.pid, error = r.error, "console attach refused; title not read");
        }
        None
    })
}

/// Set the console title through `landing`, the members of a console this
/// process has written to before, and then the candidate pids, stopping at the
/// first that attaches.
///
/// `None` means nothing would attach, so nothing was written and nothing was
/// learned. `Some` means a console was reached: `ok` says whether the title
/// actually went in, `host` is what `host_of` made of that console, and the
/// landing is every other process attached to it.
///
/// Far-to-near: the hook reports candidates ordered nearest-first (its own
/// console processes, then parent, grandparent, …). The near end is transient
/// per-hook processes holding a fresh *invisible* console (hooks are spawned
/// `CREATE_NO_WINDOW`) — a title written there is lost. The far end is GUI
/// ancestors (Windows Terminal, explorer) where attach simply fails. So walking
/// from the far end, the first successful attach is the user's shell or Claude
/// Code itself — the real terminal console.
///
/// The attached console *can* discriminate, contrary to what was once believed:
/// measured on Windows 11 on 2026-09-03, a ConPTY console reports a real
/// (invisible, 0x0) window and the consoles reporting **none** are exactly the
/// hook-side `CREATE_NO_WINDOW` ones. So the walk is kept for its own sake — it
/// costs one attach and needs no window at all — while `host_of` reads that same
/// console for the question the walk cannot answer: which of the terminal's
/// surfaces this console is rendered in.
///
/// `host_of` is asked inside the attach, which is the only place the answer
/// means anything, and under [`ATTACH_LOCK`], so it must be cheap and must not
/// block (see [`TerminalAdapter::attached_surface`](super::TerminalAdapter::attached_surface)).
pub fn write_title(landing: Option<&[u32]>, candidates: &[u32], title: &str, host_of: impl Fn(u32) -> Option<String>) -> Option<TitleWrite> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let own = std::process::id();
    with_console(attach_order(landing, candidates), |pid| {
        let ok = unsafe { SetConsoleTitleW(wide.as_ptr()) } != 0;
        let host = host_of(pid);
        Some(TitleWrite { ok, via: Some(pid), host, landing: TitleLanding::Console(console_members().into_iter().filter(|&p| p != own).collect()) })
    })
    .unwrap_or_else(|refusals| {
        for r in refusals {
            tracing::debug!(pid = r.pid, error = r.error, title, "console attach refused; title not written");
        }
        None
    })
}

/// The pids to attach through, in order: the members of a console written to
/// before, then the candidates far-to-near.
fn attach_order(landing: Option<&[u32]>, candidates: &[u32]) -> Vec<u32> {
    landing.into_iter().flatten().copied().chain(candidates.iter().rev().copied()).collect()
}

/// The processes attached to the console this process is attached to, or none
/// when the list does not fit, in which case the call stores nothing.
fn console_members() -> Vec<u32> {
    let mut buf = [0u32; 64];
    let n = unsafe { GetConsoleProcessList(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n > buf.len() {
        return Vec::new();
    }
    buf[..n].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_reaches_the_console_through_its_surviving_members_before_the_candidates() {
        // The agent (300) is dead; the shell it ran under (200) still holds the
        // console the row's last title went to.
        assert_eq!(attach_order(Some(&[200, 300]), &[300, 100]), [200, 300, 100, 300]);
        assert_eq!(attach_order(None, &[300, 100]), [100, 300], "a write goes far-to-near through the candidates alone");
    }
}
