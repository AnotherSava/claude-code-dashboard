//! Claude Code's own list of live sessions (`<claude-config>/sessions/<pid>.json`),
//! read for two questions: **which process's terminal displays a row**
//! (`tab_pid`, for titling) and **which sessions are live on this machine right
//! now** (`live_sessions`, for the `/api/agents` roster).
//!
//! The first replaced walking the hook's ancestor chain for title targets. The
//! walk answers "which processes are above the hook", which stops being the same
//! question as "which tab is the user looking at" the moment Claude Code runs a
//! conversation somewhere other than the tab: a background agent executes
//! inside a detached daemon, so its chain is entirely tty-less, and an
//! unbounded chain climbs out through whichever client spawned the daemon —
//! which is how one agent's status was written onto a neighbour's tab 558
//! times. A daemon is shared, so no walk of any shape recovers the tab.
//!
//! The second exists because the dashboard learns a session exists only when
//! that session fires a hook and restores nothing at startup, so every restart
//! empties the roster and it refills on session *activity*, not on a timer — a
//! session idle since before the restart stays invisible for as long as it
//! stays idle, which is exactly the session a caller is most likely asking
//! after. Measured across one redeploy: 9 live sessions, 1 row; six minutes
//! later, 2. The registry knows all 9 the whole time.
//!
//! The registry sidesteps the process tree entirely. Claude Code writes one
//! file per live session carrying `{pid, sessionId, cwd, kind, name, status,
//! statusUpdatedAt, …}`; `kind` is `"interactive"` for a session that owns a
//! terminal, and `"bg"` / `"daemon"` / `"daemon-worker"` for ones that do not.
//! Matching on `cwd` is what makes it answer *our* questions: a row's identity
//! is already cwd-derived (`adapters::claude::derive_chat_id`), so the session
//! sharing a row's cwd is the session sitting in that row's tab — whether or
//! not the conversation is executing there.
//!
//! Two properties keep both readings honest:
//!
//! - **A pid must still be a live Claude Code process.** Verified against
//!   `liveness::process_images`, the same image test the reaper uses — which
//!   defeats pid reuse without parsing start times, and needs one process-table
//!   snapshot rather than one `ps` per candidate. A clean exit removes the
//!   file, a `SIGKILL` or a power loss does not, and the sweep is demonstrably
//!   imperfect (orphaned `.key` files outlive their `.json` siblings). Records
//!   also carry `procStart`, which Claude Code itself compares against the live
//!   process start time — a strictly stronger check, and the available upgrade
//!   if the image test ever proves too weak. Two residual holes stay open and
//!   are not papered over: a record written from another pid namespace (a
//!   devcontainer with `~/.claude` mounted, distinguishable only by the
//!   `pidDomain` field) can collide with a live local pid, and a config dir
//!   shared with another user can have an image match confirm a session that
//!   isn't ours.
//! - **Ambiguity is resolved per reading, not once for both.** Two interactive
//!   sessions in one directory (a `--fork-session --resume` migration leaves
//!   exactly that) are two tabs and one dashboard row. Titling either would be
//!   a coin flip, so `tab_pid` answers nothing — silence is the same answer the
//!   caller already handles for a session with no terminal at all. The roster
//!   cannot answer silence: dropping the cwd would hide *both* live sessions,
//!   which is the exact blindness `live_sessions` exists to remove. It emits
//!   one row per cwd (the dashboard's row model is one row per cwd anyway),
//!   picks the record with the freshest status stamp, and reports how many
//!   collapsed. Deliberately *not* a synthesized composite ("busy if any is
//!   busy"): that reading would be present in no record.
//!
//! Both readings share one cache, so a roster request costs no extra directory
//! read and no extra process-table snapshot, and a title and a roster served
//! within one TTL cannot disagree about which processes exist. The cache holds
//! the surviving *records*; `chat_id` is derived per query rather than baked in,
//! which also means a `projects_root` change takes effect immediately instead of
//! being masked for up to a TTL.
//!
//! The file is an undocumented Claude Code internal, so every failure degrades
//! to "no answer" and the caller falls back to its previous behaviour. Absence
//! from the registry is therefore *better* evidence than absence from the hook
//! stream, but still not conclusive: a headless `claude -p` session writes no
//! record at all.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::adapters::claude::derive_chat_id;
use crate::liveness;

/// How long a read of the registry is reused before the directory is read
/// again. Sessions start and exit on human timescales while
/// `terminal_title::sync` runs on every state transition and `/api/agents` is
/// polled, so an uncached read would open ~a dozen files several times a second
/// to learn nothing.
const CACHE_TTL_MS: i64 = 5_000;

/// The subset of a registry record this module needs. Claude Code writes many
/// more fields; `serde` ignores them, so a new one upstream is not a breaking
/// change here.
///
/// **Every added field must be optional.** `read_records` drops a record that
/// fails to deserialize, so one required field that upstream renames or omits
/// would silently empty the registry — taking terminal titles down with the
/// roster.
#[derive(Deserialize)]
struct Record {
    pid: u32,
    cwd: String,
    /// `"interactive"` (owns a terminal) or `"bg"` / `"daemon"` /
    /// `"daemon-worker"` (no terminal). Absent on a record shape we don't
    /// recognize, which is treated as "not interactive".
    #[serde(default)]
    kind: Option<String>,
    /// The session's own name, as Claude Code holds it (`nameSource: "user"`
    /// when the user set it). Reported under its own key rather than folded
    /// into `display_name`, which comes from this dashboard's `CustomNamesStore`
    /// and is a different fact with a different owner.
    #[serde(default)]
    name: Option<String>,
    /// `"idle"` or `"busy"` — see `Activity` for why this never becomes a
    /// dashboard `Status`.
    #[serde(default)]
    status: Option<String>,
    /// When `status` was last written, epoch ms on *this* machine's clock (the
    /// registry is local-only, so this age carries no skew).
    #[serde(default, rename = "statusUpdatedAt")]
    status_updated_at: Option<i64>,
    /// Claude Code's own session id — the key `ChatIdRegistry` anchors a row's
    /// chat_id under. Carried so the roster can prefer the anchored id over a
    /// fresh cwd derivation; see `live_rows`.
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    /// Where this session listens for cross-session messages: a Unix socket on
    /// macOS/Linux, a named pipe (`\\.\pipe\LOCAL\cc-msg-<32hex>`) on Windows.
    ///
    /// Taken **verbatim**, never reconstructed from a template. The path is
    /// Claude Code's to choose — it falls back to `cc-socks-<uid>` when the
    /// primary directory is refused, and moves aside to `<pid>-<8hex>.sock` when
    /// a sibling pid namespace already holds the name — so a path we built from
    /// a pattern would address the wrong session, or nothing, exactly in the
    /// cases the fallbacks exist for. On Windows the shape is unverified from
    /// this machine, which is a second reason to copy rather than compose it.
    ///
    /// Read through [`SessionRegistry::inbox_for`] only; deliberately absent
    /// from [`LiveSession`], which is serialized onto the loopback roster. The
    /// address of a live agent's IPC channel is not a fact a roster reader needs.
    #[serde(default, rename = "messagingSocketPath")]
    messaging_socket_path: Option<String>,
}

/// What the registry says a session is doing — its own two words, unedited.
///
/// Deliberately **not** mapped onto `state::Status`. `Blocked`, `Waiting` and
/// `Error` exist precisely because idle/busy cannot express them, so
/// `busy -> Working` would not be coarse but false: a session parked on an
/// `AskUserQuestion` or a permission dialog has a turn in flight and is what
/// this dashboard calls `Blocked` — the one state a caller most needs not to
/// misread. `idle -> Idle` fails symmetrically, covering Done, Error and a
/// settled hand-back alike. `Unknown` absorbs both a missing field and a third
/// value upstream may add, so an unrecognized reading degrades instead of lying.
///
/// One hazard the naming has to carry: `"idle"` serializes identically to
/// `Status::Idle`, so it is the distinct *field name* and the distinct array
/// that stop a caller's `row.status ?? row.activity` from producing a confident
/// false state.
/// `Deserialize` as well as `Serialize` because a peer's registry rows cross the
/// sync wire (`sync::RegistrySync`); `Unknown` is the landing place for anything
/// a future build sends that this one does not know.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    Idle,
    Busy,
    #[serde(other)]
    Unknown,
}

impl Activity {
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("idle") => Activity::Idle,
            Some("busy") => Activity::Busy,
            _ => Activity::Unknown,
        }
    }
}

/// One live interactive session as the roster sees it: everything the registry
/// can honestly say and nothing it cannot. No label, no task boundary, no
/// dialog — those exist only in the hook stream.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveSession {
    /// Derived exactly as a hook-driven row's id is, which is what lets the
    /// roster union dedupe against hook rows by equality.
    pub chat_id: String,
    pub name: Option<String>,
    pub activity: Activity,
    /// Time since `status` was last written, or `None` when the record carries
    /// no stamp. Clamped at 0 like every other age in the roster.
    pub activity_age_ms: Option<i64>,
    /// How many interactive sessions collapsed into this one row — 1 normally,
    /// 2 after a fork migration left two tabs in one directory.
    pub sessions: usize,
    /// The session ids behind this row, so the caller can prefer an id already
    /// anchored in `ChatIdRegistry` over `chat_id`'s fresh cwd derivation.
    pub session_ids: Vec<String>,
}

/// What the registry can say about where to write a message for one row.
///
/// Five answers rather than an `Option`, because a caller that has to explain a
/// refusal needs to know *which* wall it hit: "that project runs nothing here"
/// and "two sessions run there and I will not choose" are different sentences to
/// the sender, and "the registry itself was unreadable" is our failure, not a
/// statement about the target. Collapsing them would produce the one answer this
/// module exists to never produce — an absence that was never established.
#[derive(Debug, PartialEq, Eq)]
pub enum InboxLookup {
    /// Exactly one live interactive session derives this id and publishes an
    /// inbox. `socket_path` is the record's own string, uncomposed.
    Found { pid: u32, socket_path: String },
    /// Several live interactive sessions derive this id (a `--fork-session
    /// --resume` migration leaves two). Refused, never guessed.
    Ambiguous { sessions: usize },
    /// The session is live but publishes no `messagingSocketPath` — an older
    /// Claude Code, or one whose messaging failed to bind.
    NoInbox,
    /// No live interactive session on this machine derives this id.
    NotFound,
    /// The registry directory could not be read at all.
    Unreadable,
}

/// Pure resolver behind [`SessionRegistry::inbox_for`].
fn inbox_in(records: &[Record], chat_id: &str, projects_root: Option<&str>) -> InboxLookup {
    let matches: Vec<&Record> = records.iter().filter(|r| derive_chat_id(Some(&r.cwd), projects_root) == chat_id).collect();
    match matches.as_slice() {
        [] => InboxLookup::NotFound,
        [only] => match only.messaging_socket_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(path) => InboxLookup::Found { pid: only.pid, socket_path: path.to_string() },
            None => InboxLookup::NoInbox,
        },
        many => InboxLookup::Ambiguous { sessions: many.len() },
    }
}

/// Managed state: the live interactive records as last read, and when.
#[derive(Default)]
pub struct SessionRegistry {
    cached: Mutex<Option<(i64, Vec<Record>)>>,
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
        tab_pid_in(Self::refresh(&mut cached, now)?, chat_id, projects_root)
    }

    /// Every live interactive session on this machine, one row per cwd-derived
    /// `chat_id` — or `None` when the registry could not be read at all.
    ///
    /// `Some(vec![])` and `None` are different answers and the caller must keep
    /// them apart: the first says this machine is running nothing, the second
    /// says we could not look. Collapsing them is what lets a roster assert an
    /// absence it never established.
    pub fn live_sessions(&self, projects_root: Option<&str>, now: i64) -> Option<Vec<LiveSession>> {
        let mut cached = self.cached.lock().unwrap();
        Self::refresh(&mut cached, now).map(|recs| live_rows(recs, projects_root, now))
    }

    /// Where a cross-machine message for `chat_id` must be written, or why it
    /// cannot be. Shares the same 5 s cache as the other two readings, so a
    /// message delivery costs no extra directory read and no extra
    /// process-table snapshot, and cannot disagree with the roster served
    /// alongside it about which sessions exist.
    ///
    /// Ambiguity is answered like [`tab_pid`](Self::tab_pid) and **not** like
    /// `live_sessions`: two interactive sessions in one directory are two
    /// inboxes, and the roster's freshest-status tiebreak — acceptable when the
    /// stake is which tab gets a title — would here decide *which agent reads a
    /// stranger's message*. So it refuses and says so, and the receipt reports
    /// the ambiguity rather than a coin flip.
    pub fn inbox_for(&self, chat_id: &str, projects_root: Option<&str>, now: i64) -> InboxLookup {
        let mut cached = self.cached.lock().unwrap();
        match Self::refresh(&mut cached, now) {
            Some(recs) => inbox_in(recs, chat_id, projects_root),
            None => InboxLookup::Unreadable,
        }
    }

    /// The single place the directory and the process table are read, so both
    /// accessors share one snapshot rather than racing two. `None` propagates an
    /// unreadable registry; an unreadable one is not cached, so a directory that
    /// appears later is picked up on the next call rather than after the TTL.
    fn refresh(cached: &mut Option<(i64, Vec<Record>)>, now: i64) -> Option<&[Record]> {
        let fresh = cached.as_ref().is_some_and(|(at, _)| now - at < CACHE_TTL_MS);
        if !fresh {
            let records = read_records()?;
            *cached = Some((now, live_interactive(records, liveness::process_images())));
        }
        cached.as_ref().map(|(_, recs)| recs.as_slice())
    }
}

/// `<claude-config>/sessions`, resolved the same way the transcript scan
/// resolves its own root — `CLAUDE_CONFIG_DIR` when set, else `$HOME/.claude`.
pub(crate) fn sessions_dir() -> Option<std::path::PathBuf> {
    crate::token_scan::config_dir().map(|d| d.join("sessions"))
}

/// Every parseable record in the registry directory, or `None` when the
/// directory itself could not be read.
///
/// The distinction is the point. An empty `Vec` and an unreadable directory are
/// the same value to every caller that flattens them, and the roster publishes
/// the result — so "nothing is running" and "I could not look" become one
/// answer, which is exactly the failure mode where a check that never ran reads
/// as a check that passed. Reachable in ordinary use, not just in theory: the
/// directory is absent on a machine whose Claude Code predates it, and a
/// node-based install resolves no `claude` image so every record is filtered out
/// downstream. `token_scan` resolves the same root and logs its count each pass
/// for the same reason; this does the narrower version, distinguishing only the
/// case it can act on.
fn read_records() -> Option<Vec<Record>> {
    let dir = sessions_dir()?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        tracing::warn!(dir = %dir.display(), "session registry unreadable — roster reports no answer, not an empty machine");
        return None;
    };
    Some(entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| serde_json::from_str::<Record>(&text).ok())
        .collect())
}

/// The records both readings are built from: interactive only, each pid
/// confirmed to still be a live Claude Code process.
///
/// `images` is the process-table snapshot (`None` when it could not be taken,
/// which skips the liveness check rather than dropping every candidate — a
/// missing snapshot is our failure, not evidence the sessions are dead).
/// Pure so the rules are testable without a registry on disk.
fn live_interactive(records: Vec<Record>, images: Option<HashMap<u32, String>>) -> Vec<Record> {
    records
        .into_iter()
        .filter(|r| r.kind.as_deref() == Some("interactive"))
        .filter(|r| match &images {
            Some(images) => images.get(&r.pid).is_some_and(|img| liveness::is_claude_image(img)),
            None => true,
        })
        .collect()
}

/// The title target for one row: a pid only when exactly one live interactive
/// session derives this `chat_id`. The ambiguity filter lives here, in the
/// *reading* that needs it, so the roster does not inherit a drop only titling
/// wants.
fn tab_pid_in(records: &[Record], chat_id: &str, projects_root: Option<&str>) -> Option<u32> {
    let mut found: Option<u32> = None;
    for r in records {
        if derive_chat_id(Some(&r.cwd), projects_root) != chat_id {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(r.pid);
    }
    found
}

/// One roster row per cwd-derived `chat_id`. Where several sessions collapse
/// into one row, the record with the freshest status stamp speaks for it (ties
/// broken by the lowest pid, so the pick is deterministic) and `sessions`
/// reports the collapse instead of leaving it emergent. Ordered by `chat_id`
/// so one unchanged registry serializes identically on every poll.
fn live_rows(records: &[Record], projects_root: Option<&str>, now: i64) -> Vec<LiveSession> {
    let mut by_chat: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
    for r in records {
        by_chat.entry(derive_chat_id(Some(&r.cwd), projects_root)).or_default().push(r);
    }
    by_chat
        .into_iter()
        .filter_map(|(chat_id, recs)| {
            let speaker = recs.iter().copied().max_by_key(|r| (r.status_updated_at.unwrap_or(i64::MIN), std::cmp::Reverse(r.pid)))?;
            Some(LiveSession {
                chat_id,
                name: speaker.name.clone(),
                activity: Activity::parse(speaker.status.as_deref()),
                activity_age_ms: speaker.status_updated_at.map(|t| (now - t).max(0)),
                sessions: recs.len(),
                session_ids: recs.iter().filter_map(|r| r.session_id.clone()).collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, cwd: &str, kind: Option<&str>) -> Record {
        Record { pid, cwd: cwd.to_string(), kind: kind.map(str::to_string), name: None, status: None, status_updated_at: None, session_id: None, messaging_socket_path: None }
    }

    fn named(pid: u32, cwd: &str, name: &str, status: &str, status_updated_at: i64) -> Record {
        Record {
            pid,
            cwd: cwd.to_string(),
            kind: Some("interactive".to_string()),
            name: Some(name.to_string()),
            status: Some(status.to_string()),
            status_updated_at: Some(status_updated_at),
            session_id: None,
            messaging_socket_path: None,
        }
    }

    /// Every pid is a live claude, so nothing is dropped for liveness.
    fn all_claude(pids: &[u32]) -> Option<HashMap<u32, String>> {
        Some(pids.iter().map(|p| (*p, "claude".to_string())).collect())
    }

    /// The title reading end to end, mirroring `tab_pid`'s body without a
    /// registry on disk.
    fn tab(records: Vec<Record>, chat_id: &str, projects_root: Option<&str>, images: Option<HashMap<u32, String>>) -> Option<u32> {
        tab_pid_in(&live_interactive(records, images), chat_id, projects_root)
    }

    /// The roster reading end to end, likewise.
    fn rows(records: Vec<Record>, projects_root: Option<&str>, images: Option<HashMap<u32, String>>, now: i64) -> Vec<LiveSession> {
        live_rows(&live_interactive(records, images), projects_root, now)
    }

    #[test]
    fn interactive_session_resolves_to_its_pid() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), all_claude(&[93331])), Some(93331));
    }

    /// The case that motivated the module: a background agent shares the row's
    /// cwd but owns no terminal, so it must never become a title target. Here
    /// the interactive sibling is the one that resolves.
    #[test]
    fn background_agent_never_becomes_the_target() {
        let recs = vec![
            rec(4546, "/Users/x/Projects/printlab", Some("bg")),
            rec(93331, "/Users/x/Projects/printlab", Some("interactive")),
        ];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), all_claude(&[4546, 93331])), Some(93331));
    }

    /// A bg agent with no interactive sibling leaves the row unresolved — it
    /// genuinely has no tab, and the caller writes nothing.
    #[test]
    fn lone_background_agent_resolves_to_nothing() {
        let recs = vec![rec(4546, "/Users/x/Projects/printlab", Some("bg"))];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), all_claude(&[4546])), None);
    }

    /// Two interactive sessions in one directory (a fork migration) are two
    /// tabs with one row between them. Titling either would be a coin flip, so
    /// neither resolves. Pinned as a regression: the ambiguity filter moved out
    /// of the shared cache when the roster started reading it, and the roster
    /// deliberately does *not* drop this cwd.
    #[test]
    fn ambiguous_cwd_resolves_to_no_title_target() {
        let recs = vec![
            rec(100, "/Users/x/Projects/landlord", Some("interactive")),
            rec(200, "/Users/x/Projects/landlord", Some("interactive")),
        ];
        assert_eq!(tab(recs, "landlord", Some("/Users/x/Projects"), all_claude(&[100, 200])), None);
    }

    /// A recycled pid now running something else must not inherit the tab.
    #[test]
    fn stale_pid_reused_by_another_program_is_dropped() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        let images = Some(HashMap::from([(93331u32, "node".to_string())]));
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), images), None);
    }

    /// A pid absent from the snapshot has exited; the file just hasn't been
    /// swept yet.
    #[test]
    fn dead_pid_is_dropped() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), Some(HashMap::new())), None);
    }

    /// Failing to read the process table is our problem, not evidence that
    /// every session died — resolve on the registry alone rather than going
    /// blank across the board.
    #[test]
    fn missing_process_snapshot_skips_the_liveness_check() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), None), Some(93331));
    }

    /// An unrecognized record shape is not assumed to own a terminal.
    #[test]
    fn record_without_a_kind_is_not_interactive() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", None)];
        assert_eq!(tab(recs, "printlab", Some("/Users/x/Projects"), all_claude(&[93331])), None);
    }

    /// Ids are the same derivation that names rows, so a nested cwd under
    /// `projects_root` matches the row it belongs to — in both readings.
    #[test]
    fn chat_id_matches_the_row_naming_derivation() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab/web", Some("interactive"))];
        assert_eq!(tab(recs, "printlab web", Some("/Users/x/Projects"), all_claude(&[93331])), Some(93331));
    }

    /// The roster reading carries the record's own words and an age off its own
    /// stamp — no dashboard status anywhere near it.
    #[test]
    fn a_live_session_reports_its_own_name_and_activity() {
        let recs = vec![named(93331, "/Users/x/Projects/printlab", "printlab", "busy", 900)];
        let rows = rows(recs, Some("/Users/x/Projects"), all_claude(&[93331]), 1_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id, "printlab");
        assert_eq!(rows[0].name.as_deref(), Some("printlab"));
        assert_eq!(rows[0].activity, Activity::Busy);
        assert_eq!(rows[0].activity_age_ms, Some(100));
        assert_eq!(rows[0].sessions, 1);
    }

    /// A record with no `status`, and one carrying a value upstream may add
    /// later, both degrade to `Unknown` rather than being read as idle.
    #[test]
    fn an_unreadable_status_is_unknown_not_idle() {
        let recs = vec![rec(93331, "/Users/x/Projects/printlab", Some("interactive"))];
        assert_eq!(rows(recs, Some("/Users/x/Projects"), None, 1_000)[0].activity, Activity::Unknown);
        let mut future = rec(93331, "/Users/x/Projects/printlab", Some("interactive"));
        future.status = Some("compacting".to_string());
        assert_eq!(rows(vec![future], Some("/Users/x/Projects"), None, 1_000)[0].activity, Activity::Unknown);
    }

    /// The roster's answer to the fork migration: one row, the freshest record
    /// speaking for it, and the collapse stated. Dropping the cwd (what titling
    /// does) would hide both live sessions, which is the blindness this reading
    /// exists to remove.
    #[test]
    fn two_sessions_in_one_cwd_collapse_to_one_row() {
        let recs = vec![
            named(100, "/Users/x/Projects/landlord", "old tab", "idle", 500),
            named(200, "/Users/x/Projects/landlord", "new tab", "busy", 900),
        ];
        let rows = rows(recs, Some("/Users/x/Projects"), all_claude(&[100, 200]), 1_000);
        assert_eq!(rows.len(), 1, "the dashboard's row model is one row per cwd");
        assert_eq!(rows[0].sessions, 2, "the collapse is reported, not hidden");
        assert_eq!(rows[0].name.as_deref(), Some("new tab"), "the freshest status stamp speaks for the row");
        assert_eq!(rows[0].activity, Activity::Busy);
    }

    /// The three non-interactive kinds own no terminal and are not sessions a
    /// caller can address, so none of them reaches the roster.
    #[test]
    fn only_interactive_sessions_reach_the_roster() {
        let recs = vec![
            rec(1, "/Users/x/Projects/a", Some("bg")),
            rec(2, "/Users/x/Projects/b", Some("daemon")),
            rec(3, "/Users/x/Projects/c", Some("daemon-worker")),
            rec(4, "/Users/x/Projects/d", Some("interactive")),
        ];
        let rows = rows(recs, Some("/Users/x/Projects"), all_claude(&[1, 2, 3, 4]), 1_000);
        assert_eq!(rows.iter().map(|r| r.chat_id.as_str()).collect::<Vec<_>>(), vec!["d"]);
    }

    /// The same liveness discipline the title reading has always had: an
    /// unswept record for an exited pid, and a recycled pid running something
    /// else, are both excluded from the roster.
    #[test]
    fn a_dead_or_reused_pid_is_excluded_from_the_roster() {
        let recs = vec![
            named(93331, "/Users/x/Projects/printlab", "printlab", "idle", 900),
            named(93332, "/Users/x/Projects/transcripts", "transcripts", "idle", 900),
        ];
        let images = Some(HashMap::from([(93332u32, "node".to_string())]));
        assert!(rows(recs, Some("/Users/x/Projects"), images, 1_000).is_empty());
    }

    /// A snapshot we failed to take must not empty the roster — that would turn
    /// our own failure into "nothing is running".
    #[test]
    fn a_missing_process_snapshot_still_yields_roster_rows() {
        let recs = vec![named(93331, "/Users/x/Projects/printlab", "printlab", "idle", 900)];
        assert_eq!(rows(recs, Some("/Users/x/Projects"), None, 1_000).len(), 1);
    }

    /// `statusUpdatedAt` is this machine's clock, but a record written a
    /// millisecond into our future (or by a peer's mounted config dir) must not
    /// read as a negative age.
    #[test]
    fn an_activity_age_is_clamped_at_zero() {
        let recs = vec![named(93331, "/Users/x/Projects/printlab", "printlab", "idle", 5_000)];
        assert_eq!(rows(recs, Some("/Users/x/Projects"), None, 1_000)[0].activity_age_ms, Some(0));
    }

    // -------- inbox_for --------

    fn with_inbox(pid: u32, cwd: &str, sock: Option<&str>) -> Record {
        let mut r = rec(pid, cwd, Some("interactive"));
        r.messaging_socket_path = sock.map(str::to_string);
        r
    }

    fn inbox(records: Vec<Record>, chat_id: &str, images: Option<HashMap<u32, String>>) -> InboxLookup {
        inbox_in(&live_interactive(records, images), chat_id, Some("/Users/x/Projects"))
    }

    /// The path is the record's own string, carried through untouched — never
    /// rebuilt from `cc-socks/<pid>.sock`, which Claude Code itself abandons for
    /// a uid-suffixed directory or a moved-aside name in exactly the cases the
    /// fallbacks exist for.
    #[test]
    fn an_inbox_path_is_taken_verbatim_from_the_record() {
        let recs = vec![with_inbox(93331, "/Users/x/Projects/printlab", Some("/private/tmp/cc-socks-501/93331-a1b2c3d4.sock"))];
        assert_eq!(
            inbox(recs, "printlab", all_claude(&[93331])),
            InboxLookup::Found { pid: 93331, socket_path: "/private/tmp/cc-socks-501/93331-a1b2c3d4.sock".into() }
        );
    }

    /// Two interactive sessions in one directory are two inboxes. The roster's
    /// freshest-status tiebreak decides which tab gets a title; here it would
    /// decide which agent reads a stranger's message, so this reading refuses
    /// instead — and says how many it found, so the receipt can explain itself.
    #[test]
    fn two_sessions_in_one_cwd_refuse_rather_than_choose_an_inbox() {
        let recs = vec![
            with_inbox(100, "/Users/x/Projects/landlord", Some("/tmp/cc-socks/100.sock")),
            with_inbox(200, "/Users/x/Projects/landlord", Some("/tmp/cc-socks/200.sock")),
        ];
        assert_eq!(inbox(recs, "landlord", all_claude(&[100, 200])), InboxLookup::Ambiguous { sessions: 2 });
    }

    /// A live session with no published inbox and a project with no session are
    /// different answers: the first can be retried against the same machine, the
    /// second means the caller addressed the wrong one.
    #[test]
    fn a_session_without_an_inbox_is_not_the_same_as_no_session() {
        let recs = vec![with_inbox(93331, "/Users/x/Projects/printlab", None)];
        assert_eq!(inbox(recs, "printlab", all_claude(&[93331])), InboxLookup::NoInbox);
        assert_eq!(inbox(Vec::new(), "printlab", Some(HashMap::new())), InboxLookup::NotFound);
        let blank = vec![with_inbox(93331, "/Users/x/Projects/printlab", Some("  "))];
        assert_eq!(inbox(blank, "printlab", all_claude(&[93331])), InboxLookup::NoInbox, "a blank path is no path");
    }

    /// A background agent shares the row's cwd but is not a session a caller can
    /// address, so the addressable set stays equal to the roster's.
    #[test]
    fn only_an_interactive_session_can_be_messaged() {
        let mut bg = rec(4546, "/Users/x/Projects/printlab", Some("bg"));
        bg.messaging_socket_path = Some("/tmp/cc-socks/4546.sock".into());
        assert_eq!(inbox(vec![bg], "printlab", all_claude(&[4546])), InboxLookup::NotFound);
    }

    /// A recycled pid must not hand out an inbox — the same image test the
    /// roster and the reaper already run, reached through the shared cache.
    #[test]
    fn a_reused_pid_publishes_no_inbox() {
        let recs = vec![with_inbox(93331, "/Users/x/Projects/printlab", Some("/tmp/cc-socks/93331.sock"))];
        assert_eq!(inbox(recs, "printlab", Some(HashMap::from([(93331u32, "node".to_string())]))), InboxLookup::NotFound);
    }

    /// The field name is an undocumented Claude Code internal, so it is pinned
    /// against a real record's shape rather than against our own encoder. Held
    /// as a literal fixture, not read from the live registry, so the test says
    /// the same thing on a machine with no sessions running.
    #[test]
    fn messaging_socket_path_parses_out_of_a_real_record() {
        let fixture = r#"{
            "pid": 95256,
            "sessionId": "eeeb554e-d85c-43a7-bbaf-836d299eee4a",
            "cwd": "/Users/x/Projects/printlab",
            "kind": "interactive",
            "name": "printlab",
            "status": "idle",
            "statusUpdatedAt": 1780789975389,
            "messagingSocketPath": "/tmp/cc-socks/95256.sock",
            "peerProtocol": 1,
            "peerFeatures": ["notify_idle", "reply_across_default_dirs", "artifact_yield"],
            "pidDomain": "darwin",
            "version": "2.1.251"
        }"#;
        let record: Record = serde_json::from_str(fixture).expect("a real record must parse");
        assert_eq!(record.messaging_socket_path.as_deref(), Some("/tmp/cc-socks/95256.sock"));
        assert_eq!(record.pid, 95256);
        // The field is optional for the reason the struct doc gives: a rename
        // upstream must cost the inbox, never the whole registry.
        let without = serde_json::from_str::<Record>(r#"{"pid":1,"cwd":"/x","kind":"interactive"}"#).expect("still parses");
        assert_eq!(without.messaging_socket_path, None);
    }

    /// Both readings are computed from one record slice — the property that
    /// makes them share a directory read and a process snapshot — and they part
    /// company only on ambiguity, which is the one rule each answers its own
    /// way.
    #[test]
    fn both_readings_agree_off_one_record_set() {
        let live = live_interactive(
            vec![
                rec(100, "/Users/x/Projects/landlord", Some("interactive")),
                rec(200, "/Users/x/Projects/landlord", Some("interactive")),
                rec(300, "/Users/x/Projects/printlab", Some("interactive")),
            ],
            all_claude(&[100, 200, 300]),
        );
        let root = Some("/Users/x/Projects");
        assert_eq!(tab_pid_in(&live, "printlab", root), Some(300));
        assert_eq!(tab_pid_in(&live, "landlord", root), None, "titling stays silent on ambiguity");
        let roster = live_rows(&live, root, 1_000);
        assert_eq!(roster.iter().map(|r| r.chat_id.as_str()).collect::<Vec<_>>(), vec!["landlord", "printlab"], "the roster reports both cwds, sorted");
    }
}
