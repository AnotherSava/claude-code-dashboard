//! Resolve a `chat_id` to the process whose terminal displays it, from Claude
//! Code's own live session registry (`<claude-config>/sessions/<pid>.json`).
//!
//! This replaces walking the hook's ancestor chain for title targets. The walk
//! answers "which processes are above the hook", which stops being the same
//! question as "which tab is the user looking at" the moment Claude Code runs a
//! conversation somewhere other than the tab: a background agent executes
//! inside a detached daemon, so its chain is entirely tty-less, and an
//! unbounded chain climbs out through whichever client spawned the daemon —
//! which is how one agent's status was written onto a neighbour's tab 558
//! times. A daemon is shared, so no walk of any shape recovers the tab.
//!
//! The registry sidesteps the tree entirely. Claude Code writes one file per
//! live session carrying `{pid, sessionId, cwd, kind}`; `kind` is
//! `"interactive"` for a session that owns a terminal and `"bg"` for one that
//! does not. Matching on `cwd` is what makes it answer *our* question: a row's
//! identity is already cwd-derived (`adapters::claude::derive_chat_id`), so the
//! session sharing a row's cwd is the session sitting in that row's tab —
//! whether or not the conversation is executing there.
//!
//! Two properties keep it honest:
//!
//! - **Only an unambiguous cwd resolves.** Two interactive sessions in one
//!   directory (a `--fork-session --resume` migration leaves exactly that) have
//!   two tabs and no way to choose between them, so neither is titled. Silence
//!   is the correct answer, and it is the same answer the caller already
//!   handles for a session with no terminal at all.
//! - **A pid must still be a live Claude Code process.** Verified against
//!   `liveness::process_images`, the same image test the reaper uses — which
//!   defeats pid reuse without parsing start times, and needs one process-table
//!   snapshot rather than one `ps` per candidate.
//!
//! The file is an undocumented Claude Code internal, so every failure degrades
//! to "no answer" and the caller falls back to its previous behaviour.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;

use crate::adapters::claude::derive_chat_id;
use crate::liveness;

/// How long a resolved map is reused before the directory is read again.
/// Sessions start and exit on human timescales while `terminal_title::sync`
/// runs on every state transition, so an uncached read would open ~a dozen
/// files several times a second to learn nothing.
const CACHE_TTL_MS: i64 = 5_000;

/// The subset of a registry record this module needs. Claude Code writes many
/// more fields; `serde` ignores them, so a new one upstream is not a breaking
/// change here.
#[derive(Deserialize)]
struct Record {
    pid: u32,
    cwd: String,
    /// `"interactive"` (owns a terminal) or `"bg"` (a background agent, which
    /// runs in the daemon and owns nothing). Absent on a record shape we don't
    /// recognize, which is treated as "not interactive".
    #[serde(default)]
    kind: Option<String>,
}

/// Managed state: the last resolved `chat_id -> pid` map and when it was built.
#[derive(Default)]
pub struct SessionRegistry {
    cached: Mutex<Option<(i64, HashMap<String, u32>)>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pid whose terminal displays `chat_id`, or `None` when the registry
    /// is unreadable, holds no interactive session for that row, or holds more
    /// than one.
    pub fn tab_pid(&self, chat_id: &str, projects_root: Option<&str>, now: i64) -> Option<u32> {
        let mut cached = self.cached.lock().unwrap();
        let fresh = cached.as_ref().is_some_and(|(at, _)| now - at < CACHE_TTL_MS);
        if !fresh {
            *cached = Some((now, resolve(&read_records(), projects_root, liveness::process_images())));
        }
        cached.as_ref()?.1.get(chat_id).copied()
    }
}

/// `<claude-config>/sessions`, resolved the same way the transcript scan
/// resolves its own root — `CLAUDE_CONFIG_DIR` when set, else `$HOME/.claude`.
fn sessions_dir() -> Option<std::path::PathBuf> {
    crate::token_scan::config_dir().map(|d| d.join("sessions"))
}

/// Every parseable record in the registry directory. A missing directory, an
/// unreadable file, or a file that isn't a session record all yield nothing
/// rather than an error — the caller's fallback covers it.
fn read_records() -> Vec<Record> {
    let Some(dir) = sessions_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| serde_json::from_str::<Record>(&text).ok())
        .collect()
}

/// Build the `chat_id -> pid` map: interactive records only, one per chat_id,
/// each pid confirmed to still be a live Claude Code process.
///
/// `images` is the process-table snapshot (`None` when it could not be taken,
/// which skips the liveness check rather than dropping every candidate — a
/// missing snapshot is our failure, not evidence the sessions are dead).
/// Pure so the resolution rules are testable without a registry on disk.
fn resolve(records: &[Record], projects_root: Option<&str>, images: Option<HashMap<u32, String>>) -> HashMap<String, u32> {
    let mut by_chat: HashMap<String, Vec<u32>> = HashMap::new();
    for r in records {
        if r.kind.as_deref() != Some("interactive") {
            continue;
        }
        if let Some(images) = &images {
            if !images.get(&r.pid).is_some_and(|img| liveness::is_claude_image(img)) {
                continue;
            }
        }
        by_chat.entry(derive_chat_id(Some(&r.cwd), projects_root)).or_default().push(r.pid);
    }
    by_chat.into_iter().filter(|(_, pids)| pids.len() == 1).map(|(chat_id, pids)| (chat_id, pids[0])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, cwd: &str, kind: Option<&str>) -> Record {
        Record { pid, cwd: cwd.to_string(), kind: kind.map(str::to_string) }
    }

    /// Every pid is a live claude, so nothing is dropped for liveness.
    fn all_claude(pids: &[u32]) -> Option<HashMap<u32, String>> {
        Some(pids.iter().map(|p| (*p, "claude".to_string())).collect())
    }

    #[test]
    fn interactive_session_resolves_to_its_pid() {
        let recs = [rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        let map = resolve(&recs, Some("/Users/x/Projects"), all_claude(&[93331]));
        assert_eq!(map.get("printlab"), Some(&93331));
    }

    /// The case that motivated the module: a background agent shares the row's
    /// cwd but owns no terminal, so it must never become a title target. Here
    /// the interactive sibling is the one that resolves.
    #[test]
    fn background_agent_never_becomes_the_target() {
        let recs = [
            rec(4546, "/Users/x/Projects/printlab", Some("bg")),
            rec(93331, "/Users/x/Projects/printlab", Some("interactive")),
        ];
        let map = resolve(&recs, Some("/Users/x/Projects"), all_claude(&[4546, 93331]));
        assert_eq!(map.get("printlab"), Some(&93331));
    }

    /// A bg agent with no interactive sibling leaves the row unresolved — it
    /// genuinely has no tab, and the caller writes nothing.
    #[test]
    fn lone_background_agent_resolves_to_nothing() {
        let recs = [rec(4546, "/Users/x/Projects/printlab", Some("bg"))];
        assert!(resolve(&recs, Some("/Users/x/Projects"), all_claude(&[4546])).is_empty());
    }

    /// Two interactive sessions in one directory (a fork migration) are two
    /// tabs with one row between them. Titling either would be a coin flip, so
    /// neither resolves.
    #[test]
    fn ambiguous_cwd_resolves_to_nothing() {
        let recs = [
            rec(100, "/Users/x/Projects/landlord", Some("interactive")),
            rec(200, "/Users/x/Projects/landlord", Some("interactive")),
        ];
        assert!(resolve(&recs, Some("/Users/x/Projects"), all_claude(&[100, 200])).is_empty());
    }

    /// A recycled pid now running something else must not inherit the tab.
    #[test]
    fn stale_pid_reused_by_another_program_is_dropped() {
        let recs = [rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        let images = Some(HashMap::from([(93331u32, "node".to_string())]));
        assert!(resolve(&recs, Some("/Users/x/Projects"), images).is_empty());
    }

    /// A pid absent from the snapshot has exited; the file just hasn't been
    /// swept yet.
    #[test]
    fn dead_pid_is_dropped() {
        let recs = [rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert!(resolve(&recs, Some("/Users/x/Projects"), Some(HashMap::new())).is_empty());
    }

    /// Failing to read the process table is our problem, not evidence that
    /// every session died — resolve on the registry alone rather than going
    /// blank across the board.
    #[test]
    fn missing_process_snapshot_skips_the_liveness_check() {
        let recs = [rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert_eq!(resolve(&recs, Some("/Users/x/Projects"), None).get("printlab"), Some(&93331));
    }

    /// An unrecognized record shape is not assumed to own a terminal.
    #[test]
    fn record_without_a_kind_is_not_interactive() {
        let recs = [rec(93331, "/Users/x/Projects/printlab", None)];
        assert!(resolve(&recs, Some("/Users/x/Projects"), all_claude(&[93331])).is_empty());
    }

    /// The map is keyed by the same derivation that names rows, so a nested cwd
    /// under `projects_root` matches the row it belongs to.
    #[test]
    fn chat_id_matches_the_row_naming_derivation() {
        let recs = [rec(93331, "/Users/x/Projects/printlab/web", Some("interactive"))];
        let map = resolve(&recs, Some("/Users/x/Projects"), all_claude(&[93331]));
        assert_eq!(map.get("printlab web"), Some(&93331));
    }
}
