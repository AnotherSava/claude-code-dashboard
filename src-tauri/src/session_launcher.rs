//! Starting a Claude Code session for a project that has none.
//!
//! The relay can only ever reach an agent a human already started, so
//! `InboxLookup::NotFound` has until now been the end of the story. This module
//! is the other answer: for a project this machine knows, open a real terminal
//! session in its directory, the same way the user would have.
//!
//! **The session is a first-class one, not a worker.** That is a requirement,
//! not a nicety, and it is what dictates the command each terminal adapter's
//! `TerminalAdapter::launch` runs. A session
//! spawned as a bare process would be invisible in the roster (the registry
//! keeps only `kind == "interactive"`), untitleable, and — the part that
//! matters most — unkillable: nothing in this crate can stop a Claude Code
//! session. A terminal window is the stop button, and it only exists if the
//! session is one the user can walk up to, type in, and close.
//!
//! **Who may start one.** Any sender that may message a running session: a
//! local process on the loopback route, and a peer device holding the shared
//! sync token whose name a `sync.peer_identity` entry binds to its Tailscale
//! node. A message to a running session already starts a turn with the user's
//! permission settings, hooks and files, so a start adds an agent coming into
//! existence unasked, and nothing a message could not already do. Consent to
//! an agent running in a directory is Claude Code's own trust prompt: a
//! directory never trusted is refused, never started.
//!
//! **Which directory.** A project id is derived from a directory and many
//! directories can derive one id (with `projects_root` unset it is the
//! basename), so the id is resolved against Claude Code's `~/.claude.json`
//! index of every directory a session ran in, and a start happens only where
//! exactly one existing directory derives it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::adapters::claude::derive_chat_id;
use crate::terminals::TerminalAdapter;
use crate::session_registry::InboxLookup;

/// How long a claimed start blocks another for. A start normally releases in
/// well under a second; this only bounds a handler that died mid-flight, so it
/// is long enough to cover a slow launch and short enough that a wedged claim
/// is not a permanent outage for that project.
const START_CLAIM_MS: i64 = 60_000;

/// Poll interval while waiting for a launched session to publish its inbox.
/// A full interactive `claude` was measured publishing `messagingSocketPath`
/// 0.25s after exec, so the first or second poll normally answers.
pub const POLL_INTERVAL_MS: u64 = 250;

/// How long the whole start takes, at the outside, measured from before the
/// launch rather than from the poll.
///
/// It has to bound the *sequence*, not one step of it: the launch, the poll, and
/// the terminal's two calls that check and clean up an unrealized session
/// (`launch_unrealized`, `discard_launch`) are four serial waits on one request. Budgeting only the poll let the worst case
/// add up past `sync::MESSAGE_HOP_TIMEOUT_SECS`, and a hop that times out is
/// reported `Unknown` — "may or may not have been written" — which is the one
/// answer where a retry writes twice. Worst case now, on agterm: 2s launch + the
/// remainder of this window + 2s `tree` + 2s `close`, comfortably inside 20s.
pub const START_DEADLINE_MS: i64 = 10_000;

/// Why a start did not happen. Each is a distinct thing for the sender to do
/// about it, which is why they are separate slugs rather than one refusal with
/// prose — a caller can branch on `ambiguous_project` (address a running
/// session instead) but never on `untrusted_directory` (nobody remote can fix
/// that).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StartRefusal {
    /// More than one existing directory derives the project id, so the id does
    /// not say which one was meant.
    AmbiguousProject,
    /// The directory the local picker named is not one.
    NoSuchDirectory,
    /// Claude Code has never been trusted in that directory. Starting anyway
    /// produces a live process parked on a modal prompt that publishes no
    /// registry record at all — measured at 180s with nothing — so this refuses
    /// rather than hanging on something no timeout can bound.
    UntrustedDirectory,
    /// A start for this project is already in flight.
    AlreadyStarting,
    /// This platform has no launcher wired up.
    NoLauncher,
    /// A launcher exists and was asked, and the launch failed: on macOS agterm
    /// not running or `agtermctl` refusing, on Windows the remote-session
    /// holder down, a script error, or `wsl.exe` killed at its timeout. Kept
    /// apart from `NoLauncher` because that one tells the sender this machine
    /// can never host a session, when here a later attempt may work.
    LaunchFailed,
    /// The terminal accepted the request but never gave the session a surface.
    /// On macOS libghostty refuses to create one while the display is asleep,
    /// so the session exists in agterm's model and its command never runs — no
    /// pty, no `claude`, no registry record, and nothing a poll could wait for.
    NotRealized,
    /// Nothing on this machine derives that project id, so there is no session
    /// to message and nothing to start — the address names nothing here.
    ///
    /// Its slug is the one the receiving route answers with, so a caller reads
    /// one answer whichever route it hit.
    NoSuchProject,
}

impl StartRefusal {
    /// The stable slug, for the receipt and the log line.
    pub fn slug(self) -> &'static str {
        match self {
            Self::AmbiguousProject => "start_ambiguous_project",
            Self::NoSuchDirectory => "start_no_directory",
            Self::UntrustedDirectory => "start_untrusted_directory",
            Self::AlreadyStarting => "start_already_running",
            Self::NoLauncher => "start_no_launcher",
            Self::LaunchFailed => "start_launch_failed",
            Self::NotRealized => "start_not_realized",
            // Deliberately outside the `start_*` family, and sharing the slug
            // the receiving route answers with: "nothing here derives that id"
            // is one fact about a machine, not a sub-reason a start was
            // declined, and a caller must not have to know which route it hit
            // to read the same answer.
            Self::NoSuchProject => "unknown_project",
        }
    }

    /// Prose for the human reading the receipt. Deliberately says what would
    /// have to change, and by whom — a sender cannot act on most of these.
    pub fn detail(self) -> &'static str {
        match self {
            Self::AmbiguousProject => "more than one directory on this machine derives that project id, so it does not say which one to start a session in",
            Self::NoSuchDirectory => "that directory does not exist",
            Self::UntrustedDirectory => "Claude Code has not been trusted in that project's directory, so a session would stop at a prompt nobody is there to answer",
            Self::AlreadyStarting => "a session for that project is already being started",
            Self::NoLauncher => "this machine has no terminal this dashboard can start a session in",
            Self::LaunchFailed => "the terminal was asked to start a session and failed; this machine's log says why, and a later attempt may succeed",
            Self::NotRealized => "the terminal created a session but never gave it a surface, most likely because this machine's display is asleep, so no agent was started",
            Self::NoSuchProject => "no existing directory on this machine derives that project id, so this is an address that names nothing here rather than a session that ended",
        }
    }
}

/// Claude Code's own top-level config file.
///
/// Deliberately **not** `token_scan::config_dir().join(...)`: the directory is
/// `~/.claude` while this file is `~/.claude.json` beside it, so composing from
/// the directory would look right and resolve nowhere. Under
/// `CLAUDE_CONFIG_DIR` the two do share a root, which is why the override is
/// still honoured.
fn claude_config_file() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir).join(".claude.json"));
        }
    }
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(|h| PathBuf::from(h).join(".claude.json"))
}

/// An existing directory whose derived id is the project asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StartCandidate {
    dir: String,
    /// Whether Claude Code has been trusted here.
    trusted: bool,
}

/// Claude Code's `~/.claude.json`, parsed, or `None` when it could not be read.
///
/// Its own function because every caller needs the same bytes and they need the
/// failure differently: [`check_startable`] treats an unreadable index as an untrusted directory —
/// the conservative direction — and [`project_knowledge`] must not report it as
/// an absent project at all.
fn claude_project_index() -> Option<serde_json::Value> {
    claude_config_file().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
}

/// Every directory Claude Code's index has recorded a session in, existing or
/// not, or `None` when the index could not be read.
pub fn indexed_dirs() -> Option<Vec<String>> {
    let config = claude_project_index()?;
    Some(config.get("projects").and_then(|p| p.as_object()).map(|p| p.keys().cloned().collect()).unwrap_or_default())
}

/// Whether this machine holds any directory that derives a project id.
///
/// This is the fact that separates *a session that has ended* from *an address
/// that never named anything here*, and the relay returned one refusal for both
/// until it had this: a sender reading `no_such_session` after mistyping an
/// address concluded a live agent had gone away, and told its user so twice.
///
/// `Unreadable` is a third answer for the same reason
/// [`crate::session_registry::InboxLookup::Unreadable`] is one: not having
/// looked is not a finding, and collapsing it into `Unknown` would turn our own
/// failure to read a file into a claim about the caller's address.
#[derive(Debug, PartialEq, Eq)]
pub enum ProjectKnowledge {
    /// At least one directory here derives that id, so the project exists on
    /// this machine and the absence is only of a *running* session.
    Known,
    /// The index was read and nothing in it derives that id.
    Unknown,
    /// The index could not be read, so this machine cannot say either way.
    Unreadable,
}

/// Ask [`ProjectKnowledge`] about one project id.
///
/// An unreadable index answers `Unreadable` and never `Unknown`, which is the
/// single confusion this distinction exists to avoid.
pub fn project_knowledge(project: &str, projects_root: Option<&str>) -> ProjectKnowledge {
    knowledge_in(claude_project_index().as_ref(), project, projects_root)
}

/// Pure half of [`project_knowledge`], mirroring [`candidates_in`], so all three
/// answers are reachable without a Claude Code install.
///
/// `Unreadable` is the reason this is split out rather than inlined: it is the
/// answer that must never silently become `Unknown`, and while the index read sat
/// inside the caller no test could produce it at all — the only lever was the
/// process's own `HOME`, which is global mutable state a parallel test run shares.
fn knowledge_in(index: Option<&serde_json::Value>, project: &str, projects_root: Option<&str>) -> ProjectKnowledge {
    let Some(config) = index else {
        return ProjectKnowledge::Unreadable;
    };
    if candidates_in(config, project, projects_root).is_empty() { ProjectKnowledge::Unknown } else { ProjectKnowledge::Known }
}

/// The existing directories in Claude Code's index whose derived id is
/// `project`, trusted first.
///
/// The index accumulates every directory a session was ever run in, including
/// system directories and ones that no longer exist, so it is read as the set
/// of places the id could mean and never as an allowlist: what a start needs
/// beyond membership is decided in [`resolve_dir`].
fn candidates_in(config: &serde_json::Value, project: &str, projects_root: Option<&str>) -> Vec<StartCandidate> {
    let Some(projects) = config.get("projects").and_then(|p| p.as_object()) else { return Vec::new() };
    let mut out: Vec<StartCandidate> = projects
        .iter()
        .filter(|(dir, _)| derive_chat_id(Some(dir), projects_root) == project)
        // A directory in the index that is no longer on disk is not a candidate
        // for anything; three of them are stale renames on the user's own
        // machine. This is the one filter applied, because the user cannot
        // choose a folder that is not there.
        .filter(|(dir, _)| Path::new(dir).is_dir())
        .map(|(dir, entry)| StartCandidate {
            dir: dir.clone(),
            trusted: entry.get("hasTrustDialogAccepted").and_then(serde_json::Value::as_bool).unwrap_or(false),
        })
        .collect();
    // Trusted first — the ones that will actually work — then by path, so the
    // order is stable across calls rather than following the map's iteration.
    out.sort_by(|a, b| b.trusted.cmp(&a.trusted).then_with(|| a.dir.cmp(&b.dir)));
    out
}

/// Everything about a start that can be decided before touching a terminal.
///
/// Split from the launch ([`TerminalAdapter::launch`]) so the whole policy is
/// testable without spawning anything, and so a refusal costs no process.
pub fn check_startable(project: &str, projects_root: Option<&str>, launcher: Option<Box<dyn TerminalAdapter>>) -> Result<(PathBuf, Box<dyn TerminalAdapter>), StartRefusal> {
    let dir = resolve_dir(claude_project_index().as_ref(), project, projects_root)?;
    // Asked last, so every refusal naming something about *this project* is
    // reached first. A machine with no launcher must still answer
    // `unknown_project` for an id nothing here derives, or its own missing
    // capability would mask what the caller got wrong about the address.
    let launcher = launcher.filter(|l| l.can_launch()).ok_or(StartRefusal::NoLauncher)?;
    Ok((dir, launcher))
}

/// The one directory `project` names on this machine, if it names exactly one
/// and Claude Code has been trusted there.
///
/// The ambiguity is counted over every existing directory deriving the id,
/// trusted or not: starting the one trusted directory beside an untrusted twin
/// would answer a question the id never settled.
///
/// An unreadable index is treated as an untrusted directory. It is the
/// conservative direction and the honest one: the trust decision could not be
/// established, and a start that stops at the prompt leaves an orphan session
/// on an unattended machine.
fn resolve_dir(index: Option<&serde_json::Value>, project: &str, projects_root: Option<&str>) -> Result<PathBuf, StartRefusal> {
    let index = index.ok_or(StartRefusal::UntrustedDirectory)?;
    match candidates_in(index, project, projects_root).as_slice() {
        [] => Err(StartRefusal::NoSuchProject),
        [one] if one.trusted => Ok(PathBuf::from(&one.dir)),
        [_] => Err(StartRefusal::UntrustedDirectory),
        _ => Err(StartRefusal::AmbiguousProject),
    }
}

/// One start at a time per project, so the 5s registry cache cannot turn two
/// messages into two sessions.
///
/// That race is not theoretical: `inbox_for` answers from a snapshot up to
/// `CACHE_TTL_MS` old, so a second message arriving moments after the first has
/// launched still sees `NotFound`. Two sessions in one directory make
/// `inbox_in` answer `Ambiguous` from then on — permanently, until one exits —
/// which would leave the *user's own* project unmessageable as the direct
/// result of messaging it.
#[derive(Default)]
pub struct StartGuard(Mutex<BTreeMap<String, i64>>);

impl StartGuard {
    /// Take the slot for `project`, or report that it is held. A claim older
    /// than [`START_CLAIM_MS`] is treated as abandoned rather than honoured, so
    /// a handler that died mid-launch cannot wedge a project forever.
    fn claim(&self, project: &str, now: i64) -> bool {
        let mut held = self.0.lock().unwrap();
        held.retain(|_, at| now - *at < START_CLAIM_MS);
        if held.contains_key(project) {
            return false;
        }
        held.insert(project.to_string(), now);
        true
    }

    fn release(&self, project: &str) {
        self.0.lock().unwrap().remove(project);
    }
}

/// What a start attempt settled on.
#[derive(Debug)]
pub enum StartResult {
    /// The registry now answers for this project. `started` says whether that
    /// is because we started it, or because the absence that got us here was
    /// stale and a session was already there. `hosted_name` is the started
    /// session's [`crate::terminals::Launched::hosted_name`], `None` when
    /// nothing was started or the terminal hosts nothing another machine
    /// attaches to.
    Settled { inbox: InboxLookup, started: bool, hosted_name: Option<String> },
    /// Nothing was started, and nothing about the registry changed.
    Refused(StartRefusal),
}

/// Start a session for `project` and wait for its inbox — the whole sequence,
/// in one place both routes call.
///
/// It is one function because every step of it is a way to end up with two
/// agents in one directory, and the two callers must not drift on any of them:
///
/// - **The absence is re-confirmed on a fresh read first.** The `NotFound` that
///   sends a caller here is served from a snapshot up to `CACHE_TTL_MS` old,
///   and — this is the part that makes it more than a race — the snapshot is
///   routinely filled *by the starting session itself*: a human runs `claude`,
///   its `SessionStart` hook reaches `terminal_title::sync`, which refreshes
///   the registry a moment before Claude Code writes the `<pid>.json`. For the
///   next five seconds the cache says that directory is empty while a session
///   is booting in it.
/// - **The claim is held across the wait, and released only where no launched
///   session is left unaccounted for**: on success, when the fresh re-check
///   finds a session already there, and when the launch itself fails. A
///   start that never registers — an unresolvable `claude`, an onboarding or
///   auth prompt, a display asleep — leaves the project `NotFound` forever, so
///   releasing the claim on that path means every later message opens another
///   window. Holding it lets [`START_CLAIM_MS`] bound the repeat instead, and
///   the caller's receipt already tells the sender nothing was delivered.
/// - **A launch that produced no surface is closed, not left behind.**
pub async fn start_and_wait(
    project: &str,
    projects_root: Option<&str>,
    registry: &crate::session_registry::SessionRegistry,
    guard: &StartGuard,
    launcher: Option<Box<dyn TerminalAdapter>>,
    now: i64,
) -> StartResult {
    let (dir, launcher) = match check_startable(project, projects_root, launcher) {
        Ok(checked) => checked,
        Err(refusal) => return StartResult::Refused(refusal),
    };
    start_in(project, &dir, projects_root, registry, guard, launcher, false, now).await
}

/// Open a session for the project in `dir` because the user asked for one
/// here, from the local picker (`integrations/pick-local-session.py`), and
/// wait for it as [`start_and_wait`] does.
///
/// The directory comes from the user rather than from an id, so nothing is
/// resolved, and Claude Code's trust prompt is not checked, since they are in
/// front of it to answer. The session is selected rather than started in the background,
/// since it is the thing they asked to see. A project already running answers
/// `Settled { started: false }`, which the picker answers by raising its tab.
pub async fn open_and_wait(
    dir: &Path,
    projects_root: Option<&str>,
    registry: &crate::session_registry::SessionRegistry,
    guard: &StartGuard,
    launcher: Option<Box<dyn TerminalAdapter>>,
    now: i64,
) -> (String, StartResult) {
    let project = derive_chat_id(Some(&dir.to_string_lossy()), projects_root);
    if !dir.is_absolute() || !dir.is_dir() {
        return (project, StartResult::Refused(StartRefusal::NoSuchDirectory));
    }
    let Some(launcher) = launcher.filter(|l| l.can_launch()) else {
        return (project, StartResult::Refused(StartRefusal::NoLauncher));
    };
    let result = start_in(&project, dir, projects_root, registry, guard, launcher, true, now).await;
    (project, result)
}

/// The sequence [`start_and_wait`] documents, once `dir` is known to be
/// startable. `select` puts the new session in front instead of starting it
/// in the background.
#[allow(clippy::too_many_arguments)]
async fn start_in(
    project: &str,
    dir: &Path,
    projects_root: Option<&str>,
    registry: &crate::session_registry::SessionRegistry,
    guard: &StartGuard,
    launcher: Box<dyn TerminalAdapter>,
    select: bool,
    now: i64,
) -> StartResult {
    if !guard.claim(project, now) {
        return StartResult::Refused(StartRefusal::AlreadyStarting);
    }
    // Re-confirm on a fresh read, now that we hold the slot. Anything other
    // than a definite absence is handed straight back: `Found` means the
    // absence was stale and there is nothing to start, and `Ambiguous` /
    // `NoInbox` / `Unreadable` are all states where starting would make the
    // situation worse rather than better.
    registry.invalidate();
    let confirmed = registry.inbox_for(project, projects_root, now);
    if !matches!(confirmed, InboxLookup::NotFound) {
        guard.release(project);
        return StartResult::Settled { inbox: confirmed, started: false, hosted_name: None };
    }

    // Started before the launch, so a slow terminal spends the same budget the
    // poll would have and the total stays inside the caller's hop.
    let deadline = crate::commands::now_ms() + START_DEADLINE_MS;
    let launch_dir = dir.to_path_buf();
    // The adapter blocks, so every call to it runs off the async runtime and the
    // adapter travels with the call rather than being borrowed across an await.
    let launched = tokio::task::spawn_blocking(move || {
        let result = launcher.launch(&launch_dir, select);
        (launcher, result)
    })
    .await;
    let (launcher, launched) = match launched {
        Ok((launcher, Ok(launched))) => (launcher, launched),
        Ok((launcher, Err(reason))) => {
            tracing::warn!(chat_id = %project, terminal = launcher.name(), reason = %reason, "the terminal refused to start a session");
            guard.release(project);
            return StartResult::Refused(StartRefusal::LaunchFailed);
        }
        Err(error) => {
            tracing::warn!(chat_id = %project, error = %error, "the launch task died");
            guard.release(project);
            return StartResult::Refused(StartRefusal::LaunchFailed);
        }
    };

    let inbox = await_inbox(registry, project, projects_root, deadline).await;
    if matches!(inbox, InboxLookup::NotFound) {
        // Nothing appeared. Ask the terminal whether it ever gave the session
        // something to run in: an unrealized one will never run its command, so
        // waiting longer cannot help and the empty session should not be left to
        // accumulate one row per message.
        let unrealized = match launched.key {
            Some(key) => tokio::task::spawn_blocking(move || {
                let dead = launcher.launch_unrealized(&key);
                if dead {
                    launcher.discard_launch(&key);
                }
                dead
            })
            .await
            .unwrap_or(false),
            None => false,
        };
        if unrealized {
            // The claim is deliberately still held: the display is asleep for
            // more than one message, and retrying just makes another dead row.
            return StartResult::Refused(StartRefusal::NotRealized);
        }
        // Slow rather than dead. Keep the claim so a resend cannot open a
        // second window while this one is still coming up.
        return StartResult::Settled { inbox, started: true, hosted_name: launched.hosted_name };
    }
    guard.release(project);
    StartResult::Settled { inbox, started: true, hosted_name: launched.hosted_name }
}

/// Wait for a just-started session to publish its inbox.
///
/// The cache is dropped before every poll, and that is not an optimisation to
/// skip: the read that decided to launch is itself what filled it, so an
/// unforced poll would keep answering from that same snapshot and conclude the
/// launch had failed while the session was already registered.
///
/// Whatever the registry last said is returned verbatim, including `Ambiguous`,
/// `NoInbox` and `Unreadable` — different problems from "it never appeared",
/// and flattening them into one timeout would hide, in particular, a second
/// session having shown up in that directory.
async fn await_inbox(registry: &crate::session_registry::SessionRegistry, project: &str, projects_root: Option<&str>, deadline: i64) -> InboxLookup {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        registry.invalidate();
        let seen = registry.inbox_for(project, projects_root, crate::commands::now_ms());
        if !matches!(seen, InboxLookup::NotFound) || crate::commands::now_ms() >= deadline {
            return seen;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index naming `dirs` as existing directories, each with its trust flag.
    fn index_of(dirs: &[(&str, bool)]) -> serde_json::Value {
        let projects: serde_json::Map<String, serde_json::Value> = dirs.iter().map(|(d, t)| (d.to_string(), serde_json::json!({"hasTrustDialogAccepted": t}))).collect();
        serde_json::json!({ "projects": projects })
    }

    /// A temporary tree holding `rel` directories, removed when dropped.
    struct Tree(PathBuf);
    impl Tree {
        fn new(tag: &str, rel: &[&str]) -> Self {
            let root = std::env::temp_dir().join(format!("resolve_{tag}_{}", std::process::id()));
            for r in rel {
                std::fs::create_dir_all(root.join(r)).unwrap();
            }
            Self(root)
        }
        fn path(&self, rel: &str) -> String {
            self.0.join(rel).to_string_lossy().to_string()
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One trusted directory deriving the id is the whole of what a start needs
    /// to know about the project.
    #[test]
    fn one_trusted_directory_resolves() {
        let t = Tree::new("one", &["web"]);
        let index = index_of(&[(&t.path("web"), true)]);
        assert_eq!(resolve_dir(Some(&index), "web", None), Ok(PathBuf::from(t.path("web"))));
    }

    /// Two directories deriving one id is the many-to-one case, and the id does
    /// not say which was meant. An untrusted twin still counts, or the start
    /// would pick the trusted one for a question the id never settled.
    #[test]
    fn two_directories_deriving_the_id_are_refused_even_when_one_is_untrusted() {
        let t = Tree::new("two", &["a/web", "b/web"]);
        let both = index_of(&[(&t.path("a/web"), true), (&t.path("b/web"), true)]);
        assert_eq!(resolve_dir(Some(&both), "web", None), Err(StartRefusal::AmbiguousProject));
        let twin = index_of(&[(&t.path("a/web"), true), (&t.path("b/web"), false)]);
        assert_eq!(resolve_dir(Some(&twin), "web", None), Err(StartRefusal::AmbiguousProject));
    }

    /// The id is derived the way the roster derives it, so a `projects_root`
    /// separates what the bare basename would merge.
    #[test]
    fn the_projects_root_separates_same_named_directories() {
        let t = Tree::new("root", &["a/web", "b/web"]);
        let index = index_of(&[(&t.path("a/web"), true), (&t.path("b/web"), true)]);
        let root = t.0.to_string_lossy().to_string();
        assert_eq!(resolve_dir(Some(&index), "a web", Some(&root)), Ok(PathBuf::from(t.path("a/web"))));
    }

    /// Claude Code's own trust prompt is the consent, so a directory never
    /// trusted is refused rather than started into a prompt nobody answers.
    #[test]
    fn an_untrusted_directory_is_refused() {
        let t = Tree::new("untrusted", &["web"]);
        let index = index_of(&[(&t.path("web"), false)]);
        assert_eq!(resolve_dir(Some(&index), "web", None), Err(StartRefusal::UntrustedDirectory));
        let silent = serde_json::json!({"projects": {t.path("web"): {}}});
        assert_eq!(resolve_dir(Some(&silent), "web", None), Err(StartRefusal::UntrustedDirectory), "a missing flag is not a yes");
    }

    /// Nothing deriving the id, and the id only deriving from a directory gone
    /// from disk, are the same fact: the address names nothing here.
    #[test]
    fn an_id_nothing_existing_derives_names_nothing() {
        let index = index_of(&[("/no/such/directory/zzz-gone-4f2a", true)]);
        assert_eq!(resolve_dir(Some(&index), "zzz-gone-4f2a", None), Err(StartRefusal::NoSuchProject));
        assert_eq!(resolve_dir(Some(&index), "zz-derives-nowhere-4f2a", None), Err(StartRefusal::NoSuchProject));
    }

    /// An index that could not be read cannot establish trust, so it refuses in
    /// the conservative direction rather than claiming the project is unknown.
    #[test]
    fn an_unreadable_index_refuses_as_untrusted() {
        assert_eq!(resolve_dir(None, "web", None), Err(StartRefusal::UntrustedDirectory));
    }

    /// A caller must not have to know which route it hit to read the same fact,
    /// so the same-device refusal carries the slug the receiving route answers
    /// with rather than a second name for one thing.
    #[test]
    fn nothing_derives_it_is_one_slug_across_both_routes() {
        assert_eq!(StartRefusal::NoSuchProject.slug(), "unknown_project");
    }

    /// The capability is this machine's business and the address is the
    /// caller's. Ordering the launcher check last is what stops the first from
    /// masking the second — a mistyped id has to keep answering
    /// `unknown_project` on a machine that could not have started it anyway,
    /// since that is the half the sender can act on.
    #[test]
    fn a_missing_launcher_does_not_mask_what_the_caller_got_wrong() {
        let answer = check_startable("zz-derives-nowhere-4f2a", None, None).err();
        assert!(
            matches!(answer, Some(StartRefusal::NoSuchProject) | Some(StartRefusal::UntrustedDirectory)),
            "the existence question comes first, got {answer:?}"
        );
    }

    /// The guard exists for the cache race, so the second claim must fail even
    /// though nothing has finished.
    #[test]
    fn only_one_start_per_project_is_in_flight() {
        let guard = StartGuard::default();
        assert!(guard.claim("transcripts", 1_000));
        assert!(!guard.claim("transcripts", 1_100), "a second message must not open a second session in the same directory");
        assert!(guard.claim("scheduler", 1_100), "a different project is unaffected");
        guard.release("transcripts");
        assert!(guard.claim("transcripts", 1_200), "releasing frees it for the next message");
    }

    /// A handler that died mid-launch must not take the project down with it.
    #[test]
    fn an_abandoned_claim_expires() {
        let guard = StartGuard::default();
        assert!(guard.claim("transcripts", 0));
        assert!(!guard.claim("transcripts", START_CLAIM_MS - 1));
        assert!(guard.claim("transcripts", START_CLAIM_MS), "past the window the claim is treated as abandoned");
    }

    /// Every refusal is distinguishable in the log and in the receipt. A shared
    /// slug would make "which directory did you mean" and "nothing is trusted
    /// there" the same event to `/investigate`.
    #[test]
    fn every_refusal_has_its_own_slug() {
        let all = [
            StartRefusal::AmbiguousProject,
            StartRefusal::NoSuchDirectory,
            StartRefusal::UntrustedDirectory,
            StartRefusal::AlreadyStarting,
            StartRefusal::NoLauncher,
            StartRefusal::LaunchFailed,
            StartRefusal::NotRealized,
            StartRefusal::NoSuchProject,
        ];
        // The list above is hand-written, so this match is what stops it going
        // stale: a new variant fails to compile here, which puts the author in
        // front of the list. Added because the list DID go stale — `NoSuchProject`
        // was introduced and this test kept passing without it, still claiming to
        // cover "every refusal".
        fn _every_variant_is_listed(r: StartRefusal) {
            match r {
                StartRefusal::AmbiguousProject
                | StartRefusal::NoSuchDirectory
                | StartRefusal::UntrustedDirectory
                | StartRefusal::AlreadyStarting
                | StartRefusal::NoLauncher
                | StartRefusal::LaunchFailed
                | StartRefusal::NotRealized
                | StartRefusal::NoSuchProject => {}
            }
        }
        let slugs: std::collections::BTreeSet<&str> = all.iter().map(|r| r.slug()).collect();
        assert_eq!(slugs.len(), all.len());
        // The `start_` prefix belongs to the refusals that are *about a start*,
        // which is what it makes greppable. `NoSuchProject` is deliberately
        // outside that family: it reports a fact about the machine — nothing here
        // derives that id — which the receiving route answers with the same slug,
        // so a caller reads one answer without knowing which route it hit.
        let (fact, starts): (Vec<&StartRefusal>, Vec<&StartRefusal>) = all.iter().partition(|r| matches!(**r, StartRefusal::NoSuchProject));
        assert!(starts.iter().all(|r| r.slug().starts_with("start_")), "the prefix is what makes the whole feature greppable in widget.jsonl");
        assert_eq!(fact.iter().map(|r| r.slug()).collect::<Vec<_>>(), vec!["unknown_project"], "the one non-start refusal, named for the fact rather than the attempt");
        assert!(all.iter().all(|r| !r.detail().is_empty()));

        // …and each must have a real arm in the canonical status map. Falling
        // through to its `_ => BAD_REQUEST` default is how the handler and the
        // map drifted apart before: the handler answered 404 for an untrusted
        // directory while the map said 503, and a sender that reads the status
        // before the body turns a 404 into "your peer is too old to have this
        // route" — a redeploy that would not have helped.
        for refusal in all {
            let receipt = crate::peer_message::Receipt::new(crate::peer_message::Outcome::Refused, "id", "dev/p", Some("dev")).because(refusal.slug());
            assert_ne!(
                crate::sync::receipt_status(&receipt),
                axum::http::StatusCode::BAD_REQUEST,
                "{} falls through to the default arm, so it would be reported as a malformed request",
                refusal.slug()
            );
        }
    }

    /// The deadline must leave the hop room to answer. A hop that times out is
    /// classed `Unknown`, and that is the one outcome a sender must not retry.
    #[test]
    fn the_start_deadline_fits_inside_the_hop_budget() {
        assert!(START_DEADLINE_MS < crate::sync::MESSAGE_HOP_TIMEOUT_SECS as i64 * 1_000);
    }

    /// The resolution source is Claude Code's index, so the filtering has to
    /// survive what is actually in it: system directories, stale renames, and
    /// entries never trusted.
    #[test]
    fn candidates_are_matched_by_derived_id_and_ranked_by_trust() {
        let here = std::env::temp_dir().join(format!("cand_probe_{}", std::process::id()));
        let sibling = here.join("nested");
        std::fs::create_dir_all(&sibling).unwrap();
        let (here_s, sibling_s) = (here.to_string_lossy().to_string(), sibling.to_string_lossy().to_string());
        let leaf = here.file_name().unwrap().to_string_lossy().to_string();

        let config = serde_json::json!({"projects": {
            here_s.clone(): {"hasTrustDialogAccepted": false},
            sibling_s.clone(): {"hasTrustDialogAccepted": true},
            "/no/such/directory/anywhere": {"hasTrustDialogAccepted": true},
            // Deliberately a path whose BASENAME is the id under test, so this
            // entry survives the id filter and is dropped only by the `is_dir`
            // check. The entry above it does not: `/no/such/directory/anywhere`
            // derives `anywhere`, so querying `no-such-directory-anywhere`
            // matches nothing and passes for a reason that has nothing to do with
            // the directory being gone — which is what made the assertion below
            // vacuous until a review disabled the `is_dir` filter and found the
            // test still green.
            "/no/such/directory/zzz-gone-4f2a": {"hasTrustDialogAccepted": true},
        }});

        let found = candidates_in(&config, "nested", None);
        assert_eq!(found, vec![StartCandidate { dir: sibling_s, trusted: true }]);

        let untrusted = candidates_in(&config, &leaf, None);
        assert_eq!(untrusted, vec![StartCandidate { dir: here_s, trusted: false }], "an untrusted folder is still a directory the id could mean");

        assert!(candidates_in(&config, "zzz-gone-4f2a", None).is_empty(), "a directory that is gone is not a candidate even though the index still lists it — this id DOES derive from a listed path, so only the is_dir filter can reject it");
        assert!(candidates_in(&config, "no-such-directory-anywhere", None).is_empty(), "and an id no listed path derives matches nothing, for the other reason");
        assert!(candidates_in(&serde_json::json!({}), "nested", None).is_empty(), "no index, no suggestions");

        // The three answers the two relay routes now refuse by. `Unreadable` is
        // the one that matters most and the one an inlined index read made
        // untestable: it must not read as `Unknown`, because that would turn our
        // own failure to read a file into a claim about the caller's address.
        assert_eq!(knowledge_in(Some(&config), "nested", None), ProjectKnowledge::Known, "a directory that derives the id is positive knowledge");
        assert_eq!(knowledge_in(Some(&config), "zzz-gone-4f2a", None), ProjectKnowledge::Unknown, "listed but gone from disk is not a project that exists here — which is why the refusal detail must not claim the machine never ran Claude Code there");
        assert_eq!(knowledge_in(Some(&config), "zz-derives-nowhere-4f2a", None), ProjectKnowledge::Unknown, "an id nothing derives, read from a readable index");
        assert_eq!(knowledge_in(Some(&serde_json::json!({})), "nested", None), ProjectKnowledge::Unknown, "an index with no projects map was still read");
        assert_eq!(knowledge_in(None, "nested", None), ProjectKnowledge::Unreadable, "an index we could not read is not an absent project");
        assert_ne!(knowledge_in(None, "nested", None), knowledge_in(Some(&serde_json::json!({})), "nested", None), "not having looked must stay distinguishable from having looked and found nothing");

        let _ = std::fs::remove_dir_all(&here);
    }
}
