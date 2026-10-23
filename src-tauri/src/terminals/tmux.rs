//! The WSL tmux server holding the claude repo's remote sessions, as an
//! adapter that closes and starts them.
//!
//! A remote session is a native `claude.exe` in a tmux pane, and it can run
//! with no terminal on this machine attached at all: the Mac's `⇄` tab is
//! enough to start one. agwinterm and Windows Terminal then have nothing to
//! show, so without this adapter `idle_close` finds no session to end and a
//! CLEAN agent runs on with nobody closing it. This lists those sessions by the pane title tmux holds,
//! which is the console title this dashboard writes, and ends one the way
//! agwinterm does, by typing `/exit` into an empty composer. The pane exits 0,
//! tmux destroys the session, and the Mac's `attach.sh` exits and closes its
//! own tab. A Mac asleep or with its ssh dropped at close time misses that
//! exit and reconnects on waking; the clean exit is what keeps the reconnect
//! from recreating the session, because claude exiting 0 in a session pane
//! writes the claude repo's `ended/<session>` marker and a reconnect
//! (`cc-session.sh … resume`) that finds it exits instead of creating. So the
//! close has to end the agent with exit 0: a `tmux kill-session` would make it
//! exit non-zero, write no marker, and be undone by the next reconnect.
//!
//! It also starts one, where `config.remote_session_script` names the claude
//! repo's `cc-session.sh`: run with `detach`, the script refuses while the
//! holder is down, names the session by the rule its picker uses, and creates
//! it in the holder's server with no client attached, so the agent runs with
//! no window on this machine until someone attaches. That launch names nothing
//! to check afterwards: a pane with no client has no surface to go unrealized,
//! and the script owns the session's name.
//!
//! The rest of the seam answers nothing: restore reads each session's console
//! title through the console adapter already, tmux has no context line, and
//! which tab a person reads is a question for the terminals that draw them.
//!
//! tmux is asked through `wsl.exe -e` in the default distro, which is where the
//! holder's server runs on this machine; a holder in another distro is not
//! found, and its sessions stay unclosed as they were. The list is asked on
//! every `idle_close` tick that has a due row, including a row no terminal can
//! close, so such a row costs one `wsl.exe` call a minute for as long as it
//! stays CLEAN: a warm call while the holder keeps the distro running, and on a
//! machine without the holder one that may start the distro each time.

use std::process::Command;
use std::time::{Duration, Instant};

use super::composer::{Keys, EXIT_POLL, EXIT_WAIT, KEY_PAUSE};
use super::tmux_wire::{self as wire, TmuxSession};
use super::{CloseTarget, Observation, TerminalAdapter, TerminalSession};
use crate::subprocess::RunFailure;

pub const NAME: &str = "tmux";

/// How long one `wsl.exe` call may take. A warm distro answers in a few
/// hundred milliseconds; a cold one boots first, which is why this is not two.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TmuxAdapter {
    /// `config.remote_session_script`, read when the adapter was built.
    script: Option<String>,
}

impl TmuxAdapter {
    pub fn new(script: Option<String>) -> Self {
        Self { script }
    }
}

/// Run `args` in the default WSL distro, without its login shell, and answer
/// its output or why there is none.
fn wsl(args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("wsl.exe");
    // wsl.exe writes its own errors (no such distro, the service failing) to
    // stdout and as UTF-16LE unless told otherwise, so without this a failure
    // of WSL itself reaches the log as an empty or garbled reason.
    cmd.env("WSL_UTF8", "1").arg("-e").args(args);
    crate::subprocess::run(cmd, CALL_TIMEOUT).map_err(|f| match f {
        RunFailure::TimedOut => format!("wsl.exe did not answer within {}s", CALL_TIMEOUT.as_secs()),
        RunFailure::NotRun(reason) => reason,
    })
}

/// What a failed call said: tmux's stderr, else wsl.exe's own message, which it
/// writes to stdout.
fn failure_text(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { stderr }
}

/// Every tmux session, or why the server could not be asked.
fn list() -> Result<Vec<TmuxSession>, String> {
    let out = wsl(&["sh", "-c", wire::LIST_SCRIPT])?;
    if !out.status.success() {
        return Err(format!("listing tmux sessions failed: {}", failure_text(&out)));
    }
    Ok(wire::parse_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Run one tmux command, answering its stdout or its stderr as the reason.
fn tmux(args: &[&str]) -> Result<String, String> {
    let mut full = vec!["tmux"];
    full.extend_from_slice(args);
    let out = wsl(&full)?;
    if !out.status.success() {
        return Err(format!("tmux {} failed: {}", args.first().unwrap_or(&""), failure_text(&out)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the session's composer holds, or `None` when none is on screen.
fn composer(pane: &str) -> Result<Option<String>, String> {
    tmux(&["capture-pane", "-e", "-p", "-t", pane]).map(|c| super::composer::composer_in(&wire::plain_lines(&c)))
}

impl TerminalAdapter for TmuxAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        None
    }

    fn poll(&mut self, _now_ms: i64) -> Vec<Observation> {
        Vec::new()
    }

    fn close_targets(&self) -> Option<Vec<CloseTarget>> {
        match list() {
            Ok(sessions) => Some(wire::close_targets(&sessions)),
            Err(error) => {
                tracing::debug!(terminal = NAME, %error, "tmux could not be asked for its sessions");
                None
            }
        }
    }

    /// Types `/exit` into the session's one pane the way
    /// [`super::composer::type_exit`] does for every close, once the session
    /// still passes [`wire::close_refusal`], and waits for tmux to destroy the
    /// session, which it does when the agent exits cleanly.
    fn close_session(&self, key: &str) -> Result<(), String> {
        let session = list()?.into_iter().find(|s| s.name == key).ok_or_else(|| format!("tmux session {key} is gone"))?;
        if let Some(refusal) = wire::close_refusal(&session) {
            return Err(refusal);
        }
        // `=name:` names the session's pane exactly; a bare name matches by
        // prefix, and `=name` alone is not resolved by pane-level commands.
        let pane = format!("={key}:");
        let send = |keys| {
            let args: &[&str] = match keys {
                Keys::Exit => &["-l", "/exit"],
                Keys::Enter => &["Enter"],
            };
            tmux(&[&["send-keys", "-t", pane.as_str()], args].concat()).map(|_| ())
        };
        super::composer::type_exit(|| composer(&pane), send, KEY_PAUSE)?;
        let target = format!("={key}");
        let until = Instant::now() + EXIT_WAIT;
        loop {
            std::thread::sleep(EXIT_POLL);
            if let Ok(out) = wsl(&["tmux", "has-session", "-t", &target]) {
                if !out.status.success() && wire::says_gone(&String::from_utf8_lossy(&out.stderr)) {
                    return Ok(());
                }
            }
            if Instant::now() >= until {
                return Err(format!("typed /exit, but the tmux session was still there after {}s", EXIT_WAIT.as_secs()));
            }
        }
    }

    fn can_close(&self) -> bool {
        true
    }

    fn can_launch(&self) -> bool {
        self.script.is_some()
    }

    /// `cc-session.sh <dir> windows start detach`. The directory goes as its
    /// Windows path and the script translates it, since only the script knows
    /// the project root a session is named below. `select` changes nothing: the
    /// session has no window here to put in front.
    fn launch(&self, dir: &std::path::Path, _select: bool) -> Result<Option<String>, String> {
        let script = self.script.as_deref().ok_or("config.remote_session_script is not set")?;
        let dir = dir.to_string_lossy();
        let out = wsl(&[script, &dir, "windows", "start", "detach"])?;
        if !out.status.success() {
            return Err(format!("cc-session.sh exited {}: {}", out.status.code().map_or("by signal".to_string(), |c| c.to_string()), failure_text(&out)));
        }
        Ok(None)
    }
}
