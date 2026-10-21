//! Running a helper program to completion or a deadline.
//!
//! The one copy of spawn, drain and kill every caller shares. Both pipes are
//! drained on threads while the child runs, because a child writing more than
//! the pipe buffer blocks until someone reads, which with the reads deferred to
//! after the exit is until the kill. `Command` has no timeout of its own, and a
//! wedged helper would otherwise hold its calling thread indefinitely.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Why a run produced no output to read.
#[derive(Debug)]
pub enum RunFailure {
    /// It could not be started or waited on; the text says which.
    NotRun(String),
    /// It outlived its deadline and was killed.
    TimedOut,
}

/// Run `cmd` with no stdin and both outputs captured, killing it at `timeout`.
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Output, RunFailure> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Redirecting all three stdio handles is NOT enough on Windows. The
    // dashboard is a GUI process and owns no console, so spawning a console
    // binary makes Windows allocate a fresh one for the child, which under
    // Windows Terminal is a real window that flashes open and shut: reported
    // 2026-08-31, once a minute, for `tailscale whois`.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`, spelled out rather than pulled from `winapi`,
        /// which this crate does not depend on for one constant.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|e| RunFailure::NotRun(format!("could not run {program}: {e}")))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunFailure::TimedOut);
            }
            Err(e) => return Err(RunFailure::NotRun(format!("could not wait for {program}: {e}"))),
        }
    };
    Ok(Output { status, stdout: stdout.join().unwrap_or_default(), stderr: stderr.join().unwrap_or_default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console spawn from this GUI process must carry `CREATE_NO_WINDOW` on
    /// Windows, or it flashes a terminal window at the user. Asserted over the
    /// source text, because the flash is invisible to any test that runs the
    /// child: a test process owns a console, so nothing is allocated for it.
    #[test]
    fn every_run_creates_no_console_window() {
        let src = include_str!("subprocess.rs");
        // The production half only: every string this test looks for also
        // appears in its own body.
        let src = &src[..src.find("#[cfg(test)]").expect("test module marker")];
        let at = src.find("pub fn run(").expect("run moved — update this test");
        let body = &src[at..at + src[at..].find(".spawn()").expect("spawn call")];
        assert!(body.contains("cmd.creation_flags(CREATE_NO_WINDOW)"), "run must set CREATE_NO_WINDOW before it spawns");
        assert!(body.contains("#[cfg(windows)]"), "the flag is Windows-only API and must stay cfg-gated");
    }

    #[test]
    fn a_program_that_cannot_start_is_not_run() {
        assert!(matches!(run(Command::new("this-program-does-not-exist-ccdash"), Duration::from_secs(5)), Err(RunFailure::NotRun(_))));
    }
}
