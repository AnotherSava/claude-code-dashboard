//! Writing a title to a session's controlling tty as an OSC 0 escape, which
//! every terminal rendering a tty (agterm, Terminal.app, iTerm) shows as the
//! tab's title. The macOS adapter answers
//! [`TerminalAdapter::write_title`](super::TerminalAdapter::write_title) through
//! it.
//!
//! Compiled on every platform, though only macOS calls it, because it needs
//! nothing but `std`: a Windows `cargo check` then sees a break in it, which it
//! could not while this was the `cfg(not(windows))` arm of `terminal_title`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::{TitleLanding, TitleWrite};

/// Write `title` to `landing`, a tty this process has written to before, else to
/// the controlling tty of the first candidate that has one.
///
/// Near-to-far, unlike the Windows console walk: there is no attach dance whose
/// first success must be the real console — transient per-hook pids are usually
/// dead by now (`ps` prints nothing) and Claude Code detaches every child it
/// spawns from the controlling terminal, so the hook-side pids report `??` and
/// fall through to Claude Code itself, which holds the tty of the visible tab.
/// Skipping past `??` is only safe because the chain stops at that process: an
/// all-`??` chain means this session owns no terminal, and answering `None`
/// leaves the title unwritten rather than climbing into someone else's tab.
///
/// A title written through `landing` names no pid to ask `host_of` about, so its
/// host is unknown.
pub fn write_title(landing: Option<&str>, candidates: &[u32], title: &str, host_of: impl Fn(u32) -> Option<String>) -> Option<TitleWrite> {
    if let Some(tty) = landing.filter(|tty| write_tty(tty, title)) {
        return Some(TitleWrite { ok: true, via: None, host: None, landing: TitleLanding::Tty(tty.to_string()) });
    }
    for &pid in candidates {
        let Ok(out) = std::process::Command::new("ps").args(["-o", "tty=", "-p", &pid.to_string()]).output() else { continue };
        let tty_raw = String::from_utf8_lossy(&out.stdout);
        let tty = tty_raw.trim();
        if tty.is_empty() || tty.starts_with('?') {
            continue;
        }
        if write_tty(tty, title) {
            return Some(TitleWrite { ok: true, via: Some(pid), host: host_of(pid), landing: TitleLanding::Tty(tty.to_string()) });
        }
    }
    None
}

/// Write an OSC 0 title escape to the tty device `tty`, as `ps` names it.
fn write_tty(tty: &str, title: &str) -> bool {
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).open(format!("/dev/{tty}")).is_ok_and(|mut dev| dev.write_all(format!("\x1b]0;{title}\x07").as_bytes()).is_ok())
}
