//! Talking to agterm, the macOS terminal, over its `agtermctl` control socket.
//!
//! Transport only — the binary lookup, the kill timeout, and the JSON envelope.
//! Two callers share it: `session_launcher`, which creates a session for a
//! project that has none, and `terminals::agterm`, which asks which session the
//! user is sitting in. They must not each own a copy: the binary lookup has to
//! keep working from a GUI app that inherits no shell profile, and the timeout is
//! what stops a wedged terminal from holding a dashboard thread forever.
//!
//! Compiled on macOS, and for tests everywhere: agterm does not exist on Windows,
//! so the transport is gated to macOS item by item and has no counterpart branch
//! to keep compiling — no `#[cfg(not(macos))]` stub of the kind the
//! `verify_cfg_gated_platform_branches` memory warns can rot unseen. What remains
//! in a Windows test build is the pure reading of agterm's answers, which
//! `terminals::agterm`'s own tests need wherever they run.

#![cfg(any(target_os = "macos", test))]

#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// How long any one `agtermctl` call may take before it is killed.
///
/// `Command` has no timeout and agtermctl talks to a control socket, so a
/// wedged agterm would otherwise hold the calling thread indefinitely — the
/// same hazard, and the same remedy, as `tailnet::whois_uncached`. Kept well
/// under `session_launcher::START_DEADLINE_MS` so a hung launcher cannot eat the
/// whole budget.
#[cfg(target_os = "macos")]
const AGTERMCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "macos")]
fn agterm_bin() -> Option<PathBuf> {
    // The bundle path first, for the same reason `tailnet::tailscale_bin`
    // spells its own out: a Tauri app on macOS inherits no shell profile, so
    // the Homebrew symlink on PATH is not reachable from here.
    let candidates = ["/Applications/agterm.app/Contents/MacOS/agtermctl", "/opt/homebrew/bin/agtermctl", "/usr/local/bin/agtermctl"];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Run one `agtermctl` command and parse its JSON answer, or `None` if it could
/// not be run, timed out, failed, or answered `ok: false`.
#[cfg(target_os = "macos")]
pub(crate) fn agtermctl(args: &[&str]) -> Option<serde_json::Value> {
    use std::process::{Command, Stdio};

    let bin = agterm_bin()?;
    let mut child = Command::new(&bin).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn().ok()?;
    let deadline = std::time::Instant::now() + AGTERMCTL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(?args, "agtermctl timed out");
                return None;
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // `ok` is agterm's own verdict on the request. It says the request was
    // served, never that the session it names is alive — see `LaunchHandle`.
    value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false).then_some(value)
}

/// Run one `agtermctl` command and answer either its JSON or the reason it
/// failed, for a caller that owes a reason rather than a `None`.
///
/// [`agtermctl`] collapses a spawn failure, the kill, a non-zero exit and an
/// explicit `ok: false` into one `None`, and discards stderr. That is the right
/// shape for the two callers that act on an answer or give up, and the wrong one
/// for `TerminalAdapter::write_label`, whose `Err` is printed on the
/// `label_write` log line: a context agterm refused — over 256 UTF-8 bytes, a
/// control character, an ambiguous target — would otherwise be logged as a
/// failure with nothing saying which. agterm's own `error` string is served
/// verbatim because it is already specific (`context must be at most 256 UTF-8
/// bytes`), and inventing a wording here would mean maintaining a second copy of
/// agterm's rules.
#[cfg(target_os = "macos")]
pub(crate) fn agtermctl_checked(args: &[&str]) -> Result<serde_json::Value, String> {
    use std::process::{Command, Stdio};

    let bin = agterm_bin().ok_or("agtermctl not found")?;
    let mut child = Command::new(&bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run agtermctl: {e}"))?;
    let deadline = std::time::Instant::now() + AGTERMCTL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("agtermctl timed out".to_string());
            }
            Err(e) => return Err(format!("could not wait for agtermctl: {e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("could not read agtermctl: {e}"))?;
    let parsed: Option<serde_json::Value> = serde_json::from_slice(&out.stdout).ok();
    // agterm answers a refusal as `{"ok":false,"error":"…"}` on stdout with exit
    // 0, so the envelope is read before the exit status is judged.
    if let Some(value) = parsed {
        if value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(value);
        }
        if let Some(reason) = value.get("error").and_then(serde_json::Value::as_str) {
            return Err(reason.to_string());
        }
    }
    let code = out.status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!("agtermctl exit {code}: {}", stderr.lines().next().unwrap_or("no output").trim()))
}

/// The ids of every *open* window in a `window list --json` answer.
///
/// Closed windows are listed too, and `tree --window <closed>` errors with
/// "window not open", so they are filtered out here rather than costing a failed
/// subprocess each. Pure and fixture-pinned, like [`session_nodes`].
pub(crate) fn open_window_ids(list: &serde_json::Value) -> Vec<String> {
    open_windows(list).filter_map(|w| w.get("id").and_then(serde_json::Value::as_str)).map(str::to_string).collect()
}

/// Every *open* window node in a `window list --json` answer. A window with no
/// `open` key counts as open, so this and every reader built on it agree about
/// which windows exist.
pub(crate) fn open_windows(list: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    list.get("result")
        .and_then(|r| r.get("windows"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|w| w.get("open").and_then(serde_json::Value::as_bool) != Some(false))
}

/// Find a session node by id in an `agtermctl tree --json` answer.
///
/// Pure, and separate from the query, so the shape this depends on is pinned by
/// a test rather than by a live terminal.
pub(crate) fn session_node<'a>(tree: &'a serde_json::Value, id: &str) -> Option<&'a serde_json::Value> {
    session_nodes(tree).find(|s| s.get("id").and_then(serde_json::Value::as_str) == Some(id))
}

/// Every session node in a `tree --json` answer, flattened across workspaces.
pub(crate) fn session_nodes(tree: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    tree.get("result")
        .and_then(|r| r.get("tree"))
        .and_then(|t| t.get("workspaces"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| w.get("sessions").and_then(serde_json::Value::as_array))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The realized check reads a real `agtermctl tree --json` shape — captured
    /// from the live 0.25.0 CLI — so a schema change breaks a test here rather
    /// than silently making every unrealized session look healthy.
    #[test]
    fn a_session_node_is_found_across_workspaces() {
        let tree: serde_json::Value = serde_json::json!({
            "result": {"tree": {"workspaces": [
                {"name": "common", "sessions": [{"id": "A57195F5", "cwd": "/p/agterm", "realized": true}]},
                {"name": "apps", "sessions": [
                    {"id": "D0558931", "cwd": "/p/dash", "realized": true},
                    {"id": "C257D880", "cwd": "/tmp/probe", "realized": false},
                ]},
            ]}}
        });
        assert_eq!(session_node(&tree, "C257D880").and_then(|n| n.get("realized")).and_then(serde_json::Value::as_bool), Some(false));
        assert_eq!(session_node(&tree, "A57195F5").and_then(|n| n.get("realized")).and_then(serde_json::Value::as_bool), Some(true));
        assert!(session_node(&tree, "NOPE").is_none());
        assert!(session_node(&serde_json::json!({"ok": true}), "C257D880").is_none(), "a shape we do not recognize yields no answer rather than a wrong one");
    }

    #[test]
    fn only_open_windows_are_listed() {
        // `window list` reports closed windows too, and `tree --window <closed>`
        // errors with "window not open" — so filtering here saves one failed
        // subprocess per closed window on every poll.
        let list = serde_json::json!({"ok": true, "result": {"windows": [
            {"id": "W1", "name": "main", "open": true, "active": true},
            {"id": "W2", "name": "side", "open": true, "active": false},
            {"id": "W3", "name": "parked", "open": false, "active": false},
        ]}});
        assert_eq!(open_window_ids(&list), vec!["W1".to_string(), "W2".to_string()]);
    }

    #[test]
    fn an_unreadable_window_list_yields_none_rather_than_a_guess() {
        assert!(open_window_ids(&serde_json::json!({"ok": true})).is_empty());
        // A window with no `open` key is treated as open: the field is documented
        // as present, and assuming "closed" on an unrecognized shape would silently
        // stop reading every window.
        let no_flag = serde_json::json!({"ok": true, "result": {"windows": [{"id": "W1"}]}});
        assert_eq!(open_window_ids(&no_flag), vec!["W1".to_string()]);
    }
}
