//! Starting a Claude Code session for a project that has none.
//!
//! The relay can only ever reach an agent a human already started, so
//! `InboxLookup::NotFound` has until now been the end of the story. This module
//! is the other answer: for a project its owner has explicitly listed, open a
//! real terminal session in that directory, the same way they would have.
//!
//! **The session is a first-class one, not a worker.** That is a requirement,
//! not a nicety, and it is what dictates the command shape below. A session
//! spawned as a bare process would be invisible in the roster (the registry
//! keeps only `kind == "interactive"`), untitleable, and — the part that
//! matters most — unkillable: nothing in this crate can stop a Claude Code
//! session. A terminal window is the stop button, and it only exists if the
//! session is one the user can walk up to, type in, and close.
//!
//! **The grant.** Listing a project here is a standing invitation. From then on
//! any agent on any device holding the shared sync token can, at any hour and
//! with nobody at this computer, open a terminal here and start Claude Code in
//! that directory — with these permission settings, these hooks, these
//! credentials and this file access. It is not the approval of a message; it is
//! the approval that a session may come into existence without you. The device
//! half of the sender's identity is attestable and is required to be
//! (`tailnet::bound`, an explicit `sync.peer_identity` entry — not merely
//! `Attested`, which also accepts an unbound name that happens to match the
//! node's own and so lets a token-holder self-attest); the agent half is checked by
//! nothing and cannot be, since the sending route is loopback and
//! unauthenticated by design. List only directories you would be content to
//! find an agent already working in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::adapters::claude::derive_chat_id;
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
/// the two `agtermctl` calls that check and clean up an unrealized session are
/// four serial waits on one request. Budgeting only the poll let the worst case
/// add up past `sync::MESSAGE_HOP_TIMEOUT_SECS`, and a hop that times out is
/// reported `Unknown` — "may or may not have been written" — which is the one
/// answer where a retry writes twice. Worst case now: 2s launch + the remainder
/// of this window + 2s `tree` + 2s `close`, comfortably inside 20s.
pub const START_DEADLINE_MS: i64 = 10_000;

/// Why a start did not happen. Each is a distinct thing for the sender to do
/// about it, which is why they are separate slugs rather than one refusal with
/// prose — a caller can branch on `not_listed` (ask the owner to add it) but
/// never on `untrusted_directory` (nobody remote can fix that).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StartRefusal {
    /// The project is not in `auto_start.json`. The default state, and the
    /// answer for every project the owner has not opted in.
    NotListed,
    /// The listed directory does not derive the id it is filed under, so the
    /// entry cannot mean what it says.
    PathMismatch,
    /// Listed, but the directory is not there any more.
    NoSuchDirectory,
    /// Claude Code has never been trusted in that directory. Starting anyway
    /// produces a live process parked on a modal prompt that publishes no
    /// registry record at all — measured at 180s with nothing — so this refuses
    /// rather than hanging on something no timeout can bound.
    UntrustedDirectory,
    /// A start for this project is already in flight.
    AlreadyStarting,
    /// This platform has no launcher wired up, or its terminal is not running.
    NoLauncher,
    /// The terminal accepted the request but never gave the session a surface.
    /// On macOS libghostty refuses to create one while the display is asleep,
    /// so the session exists in agterm's model and its command never runs — no
    /// pty, no `claude`, no registry record, and nothing a poll could wait for.
    NotRealized,
}

impl StartRefusal {
    /// The stable slug, for the receipt and the log line.
    pub fn slug(self) -> &'static str {
        match self {
            Self::NotListed => "start_not_listed",
            Self::PathMismatch => "start_path_mismatch",
            Self::NoSuchDirectory => "start_no_directory",
            Self::UntrustedDirectory => "start_untrusted_directory",
            Self::AlreadyStarting => "start_already_running",
            Self::NoLauncher => "start_no_launcher",
            Self::NotRealized => "start_not_realized",
        }
    }

    /// Prose for the human reading the receipt. Deliberately says what would
    /// have to change, and by whom — a sender cannot act on most of these.
    pub fn detail(self) -> &'static str {
        match self {
            Self::NotListed => "no session is running for that project and its owner has not listed it as startable",
            Self::PathMismatch => "that project is listed as startable but its configured directory derives a different project id",
            Self::NoSuchDirectory => "that project is listed as startable but its configured directory does not exist",
            Self::UntrustedDirectory => "that project is listed as startable but Claude Code has not been trusted in its directory, so a session would stop at a prompt nobody is there to answer",
            Self::AlreadyStarting => "a session for that project is already being started",
            Self::NoLauncher => "this machine has no terminal this dashboard can start a session in",
            Self::NotRealized => "the terminal created a session but never gave it a surface, most likely because this machine's display is asleep, so no agent was started",
        }
    }
}

/// The directory to start `project` in, if the config says one and the config
/// means it.
///
/// The second half is the point. `auto_start.json` is hand-written, its keys are
/// `derive_chat_id` outputs, and a wrong key is invisible: filing
/// `/Users/me/Projects/scheduler` under `transcripts` would silently send every
/// message addressed to one project into the other's directory. Re-deriving the
/// id from the listed path and requiring it to match makes the entry check
/// itself, so the only way to start a directory is to have named it under the
/// id it actually produces.
pub fn listed_dir(project: &str, auto_start: &BTreeMap<String, String>, projects_root: Option<&str>) -> Result<PathBuf, StartRefusal> {
    let dir = auto_start.get(project).map(|d| d.trim()).filter(|d| !d.is_empty()).ok_or(StartRefusal::NotListed)?;
    if derive_chat_id(Some(dir), projects_root) != project {
        return Err(StartRefusal::PathMismatch);
    }
    Ok(PathBuf::from(dir))
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

/// Pure half of the trust check: does this config blob record `dir` as trusted?
///
/// Absence is **not** trusted. The map is an undocumented Claude Code internal
/// that accumulates every directory a session ever ran in, so a missing entry
/// means "never opened here", which is exactly the case that would stop at the
/// trust prompt. Comparison is on the path as written after trailing-separator
/// normalisation only — no canonicalisation, because a symlinked alias of a
/// trusted directory is a *different* key to Claude Code too, and guessing
/// otherwise would claim a trust decision that was never made.
fn trusted_in(config: &serde_json::Value, dir: &Path) -> bool {
    let Some(projects) = config.get("projects").and_then(|p| p.as_object()) else { return false };
    let wanted = normalize_dir(&dir.to_string_lossy());
    projects
        .iter()
        .any(|(k, v)| normalize_dir(k) == wanted && v.get("hasTrustDialogAccepted").and_then(serde_json::Value::as_bool).unwrap_or(false))
}

/// Separator- and trailing-slash-insensitive form, so a `D:\p\x` entry and a
/// `D:/p/x` one are the same directory. Case is left alone: both filesystems
/// here are case-insensitive but the ids derived from these paths are not, and
/// folding here would let a differently-cased entry authorise a start under an
/// id it does not produce.
fn normalize_dir(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

/// A directory this machine could plausibly start `project` in, offered to the
/// user when they are asked to approve one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StartCandidate {
    pub dir: String,
    /// Whether Claude Code has been trusted here. An untrusted candidate is
    /// still shown, with this false, rather than filtered out: the user is
    /// looking for a folder they recognise, and silently omitting the right one
    /// would leave them concluding the dashboard cannot see it. What it needs is
    /// a sentence telling them to open it in Claude Code once, not a hidden row.
    pub trusted: bool,
}

/// Directories on this machine whose derived id is `project`.
///
/// The source is Claude Code's `~/.claude.json` projects map — the only index
/// anywhere that holds real absolute paths. It is emphatically **not** an
/// allowlist and is never used as one: it accumulates every directory a session
/// was ever run in, including `C:\WINDOWS\system32` and directories that no
/// longer exist. As a *suggestion* source it is exactly right, because the
/// question here is "which of these did you mean", asked of the one party who
/// can answer, and the answer still has to pass every check in
/// [`check_startable`] afterwards.
pub fn candidates_for(project: &str, projects_root: Option<&str>) -> Vec<StartCandidate> {
    let Some(config) = claude_config_file().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else {
        return Vec::new();
    };
    candidates_in(&config, project, projects_root)
}

/// Pure half of [`candidates_for`], so the filtering is testable without a
/// Claude Code install.
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
/// Split from [`launch`] so the whole policy is testable without spawning
/// anything, and so a refusal costs no process.
pub fn check_startable(project: &str, auto_start: &BTreeMap<String, String>, projects_root: Option<&str>) -> Result<PathBuf, StartRefusal> {
    let dir = listed_dir(project, auto_start, projects_root)?;
    if !dir.is_dir() {
        return Err(StartRefusal::NoSuchDirectory);
    }
    let config = claude_config_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    match config {
        Some(config) if trusted_in(&config, &dir) => Ok(dir),
        // An unreadable config file is treated the same as an untrusted
        // directory. It is the conservative direction and the honest one: we
        // could not establish the trust decision, and a start that stops at the
        // prompt leaves an orphan window on an unattended machine.
        _ => Err(StartRefusal::UntrustedDirectory),
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
    /// stale and a session was already there.
    Settled { inbox: InboxLookup, started: bool },
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
/// - **The claim is held across the wait, and released only on success.** A
///   start that never registers — an unresolvable `claude`, an onboarding or
///   auth prompt, a display asleep — leaves the project `NotFound` forever, so
///   releasing the claim on that path means every later message opens another
///   window. Holding it lets [`START_CLAIM_MS`] bound the repeat instead, and
///   the caller's receipt already tells the sender nothing was delivered.
/// - **A launch that produced no surface is closed, not left behind.**
pub async fn start_and_wait(
    project: &str,
    auto_start: &BTreeMap<String, String>,
    projects_root: Option<&str>,
    registry: &crate::session_registry::SessionRegistry,
    guard: &StartGuard,
    now: i64,
) -> StartResult {
    let dir = match check_startable(project, auto_start, projects_root) {
        Ok(dir) => dir,
        Err(refusal) => return StartResult::Refused(refusal),
    };
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
        return StartResult::Settled { inbox: confirmed, started: false };
    }

    // Started before the launch, so a slow `agtermctl` spends the same budget
    // the poll would have and the total stays inside the caller's hop.
    let deadline = crate::commands::now_ms() + START_DEADLINE_MS;
    let launch_dir = dir.clone();
    let launched = tokio::task::spawn_blocking(move || launch(&launch_dir)).await.unwrap_or(Err(StartRefusal::NoLauncher));
    let handle = match launched {
        Ok(handle) => handle,
        Err(refusal) => {
            guard.release(project);
            return StartResult::Refused(refusal);
        }
    };

    let inbox = await_inbox(registry, project, projects_root, deadline).await;
    if matches!(inbox, InboxLookup::NotFound) {
        // Nothing appeared. Ask the terminal whether it ever gave the session a
        // surface: an unrealized one will never run its command, so waiting
        // longer cannot help and the empty session should not be left to
        // accumulate one row per message.
        if handle.is_unrealized() {
            handle.close();
            // The claim is deliberately still held: the display is asleep for
            // more than one message, and retrying just makes another dead row.
            return StartResult::Refused(StartRefusal::NotRealized);
        }
        // Slow rather than dead. Keep the claim so a resend cannot open a
        // second window while this one is still coming up.
        return StartResult::Settled { inbox, started: true };
    }
    guard.release(project);
    StartResult::Settled { inbox, started: true }
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

/// Open a real terminal session running `claude` in `dir`.
///
/// macOS drives agterm's control socket. The command is
/// `zsh -ilc 'claude; exec zsh -i'`, and every part of that earns its place:
///
/// - `--command` rather than creating a shell and typing into it. agterm's own
///   guidance is explicit that `session type` "is not a launcher" — its
///   keystrokes land in a shared line buffer that a concurrent writer can
///   corrupt.
/// - `zsh -ilc`, not `-lc`. A GUI app inherits the launchd `PATH`
///   (`/usr/bin:/bin:/usr/sbin:/sbin`), and the user's `claude` lives in
///   `~/.local/bin`, which `.zshrc` prepends — a login-but-non-interactive
///   shell never sources it and resolves `claude: none`. `-i` also picks up the
///   user's own `claude` shell function, so the session is started with the
///   same `--continue` and the same title handling as one started by hand.
/// - `exec zsh -i` after it. Bare `--command` closes the session when the
///   command exits, and `--wait` only holds a dead session on a press-any-key
///   prompt; neither leaves something to walk up to. Exec'ing an interactive
///   shell means quitting claude drops to a live prompt in the project
///   directory, exactly as a hand-started session does.
/// - `--no-select`, so a message never steals focus from whoever is at the
///   machine. Verified not to cost anything: a `--no-select` session is
///   realized immediately, pty and all.
/// - no `--name`. agterm's `--name` sets a `customName` that permanently
///   outranks the OSC title, which would make that row dark for the dashboard's
///   own status writes forever. The user's `claude` function passes its own
///   `--name` to *claude*, which is a different thing and is preserved.
///
/// Windows is not wired up yet and refuses [`StartRefusal::NoLauncher`]. The
/// launch mechanism there is known — a plain `Command::new` from this
/// console-less GUI process makes Windows allocate a console, which under
/// Windows Terminal is a real window (that is the flash `CREATE_NO_WINDOW`
/// suppresses for `tailscale whois`) — but which shell wrapper leaves a live
/// prompt when claude exits depends on how that machine starts claude, and is
/// not yet established.
fn launch(dir: &Path) -> Result<LaunchHandle, StartRefusal> {
    #[cfg(target_os = "macos")]
    {
        launch_macos(dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
        Err(StartRefusal::NoLauncher)
    }
}

/// A session a launcher created, so the caller can ask whether it came to life
/// and clean it up when it did not.
///
/// It exists because the launcher's `ok` answer is weaker than it looks: on
/// macOS agterm reports success for a session that exists **in its model**, and
/// libghostty refuses to give one a surface while the display is asleep. Such a
/// session never runs its command, so no `claude` starts, no registry record
/// appears, and a caller with nothing but `ok` would wait out its deadline and
/// leave a dead row behind — one more per message, forever.
struct LaunchHandle {
    #[cfg(target_os = "macos")]
    session_id: Option<String>,
}

impl LaunchHandle {
    /// Whether the terminal is known to have created no surface for this
    /// session.
    ///
    /// Positive knowledge only: an unanswerable query returns `false`, because
    /// the caller acts on this by giving up, and giving up on a session that is
    /// merely slow would be a worse mistake than waiting for one that is dead.
    fn is_unrealized(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            let Some(id) = self.session_id.as_deref() else { return false };
            let Some(tree) = agtermctl(&["tree", "--json"]) else { return false };
            return session_node(&tree, id).is_some_and(|n| n.get("realized").and_then(serde_json::Value::as_bool) == Some(false));
        }
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Remove a session that never came to life. Best-effort: a failure here
    /// leaves one stale row, which is strictly better than the alternative of
    /// not trying.
    fn close(&self) {
        #[cfg(target_os = "macos")]
        if let Some(id) = self.session_id.as_deref() {
            let _ = agtermctl(&["session", "close", "--target", id, "--json"]);
        }
    }
}

/// The command handed to agterm. Shared with the test that pins it, so the
/// shape above cannot drift from the shape documented on [`launch`].
#[cfg(target_os = "macos")]
const AGTERM_COMMAND: &str = "zsh -ilc 'claude; exec zsh -i'";

/// How long any one `agtermctl` call may take before it is killed.
///
/// `Command` has no timeout and agtermctl talks to a control socket, so a
/// wedged agterm would otherwise hold the calling thread indefinitely — the
/// same hazard, and the same remedy, as `tailnet::whois_uncached`. Kept well
/// under [`START_DEADLINE_MS`] so a hung launcher cannot eat the whole budget.
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
fn agtermctl(args: &[&str]) -> Option<serde_json::Value> {
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
    // served, never that the session it names is alive — see [`LaunchHandle`].
    value.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false).then_some(value)
}

/// Find a session node by id in an `agtermctl tree --json` answer.
///
/// Pure, and separate from the query, so the shape this depends on is pinned by
/// a test rather than by a live terminal.
#[cfg(target_os = "macos")]
fn session_node<'a>(tree: &'a serde_json::Value, id: &str) -> Option<&'a serde_json::Value> {
    tree.get("result")?
        .get("tree")?
        .get("workspaces")?
        .as_array()?
        .iter()
        .filter_map(|w| w.get("sessions")?.as_array())
        .flatten()
        .find(|s| s.get("id").and_then(serde_json::Value::as_str) == Some(id))
}

#[cfg(target_os = "macos")]
fn launch_macos(dir: &Path) -> Result<LaunchHandle, StartRefusal> {
    let answer = agtermctl(&["session", "new", "--cwd", &dir.to_string_lossy(), "--no-select", "--json", "--command", AGTERM_COMMAND])
        .ok_or(StartRefusal::NoLauncher)?;
    // The id is what makes the session addressable afterwards. Its absence is
    // not fatal — the session was created — but it costs the realized check and
    // the cleanup, so it is worth a line in the log.
    let session_id = answer.get("result").and_then(|r| r.get("id")).and_then(serde_json::Value::as_str).map(str::to_string);
    if session_id.is_none() {
        tracing::warn!("agterm created a session but returned no id; it cannot be checked or cleaned up");
    }
    Ok(LaunchHandle { session_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// The default is off, and off means every project.
    #[test]
    fn an_empty_listing_starts_nothing() {
        assert_eq!(listed_dir("transcripts", &BTreeMap::new(), None), Err(StartRefusal::NotListed));
    }

    #[test]
    fn a_listed_project_resolves_to_its_directory() {
        let cfg = listing(&[("transcripts", "/Users/me/Projects/transcripts")]);
        assert_eq!(listed_dir("transcripts", &cfg, None), Ok(PathBuf::from("/Users/me/Projects/transcripts")));
        assert_eq!(listed_dir("scheduler", &cfg, None), Err(StartRefusal::NotListed), "listing one project does not list its neighbours");
    }

    /// The check that makes a hand-written map safe. Without it a mistyped key
    /// silently aims one project's messages at another's directory, and every
    /// downstream signal — the row, the title, the history — would agree with
    /// the mistake.
    #[test]
    fn a_directory_filed_under_the_wrong_id_is_refused() {
        let cfg = listing(&[("transcripts", "/Users/me/Projects/scheduler")]);
        assert_eq!(listed_dir("transcripts", &cfg, None), Err(StartRefusal::PathMismatch));
    }

    /// The id is derived the same way the roster derives it, so a `projects_root`
    /// deployment must file its entries under the root-relative form.
    #[test]
    fn the_id_is_derived_under_the_configured_projects_root() {
        let cfg = listing(&[("games achievement overlay", "/Users/me/Projects/games/achievement-overlay")]);
        assert_eq!(
            listed_dir("games achievement overlay", &cfg, Some("/Users/me/Projects")),
            Ok(PathBuf::from("/Users/me/Projects/games/achievement-overlay"))
        );
        assert_eq!(
            listed_dir("games achievement overlay", &cfg, None),
            Err(StartRefusal::PathMismatch),
            "without the root the same path derives the bare basename, and the entry no longer means what it says"
        );
    }

    #[test]
    fn a_windows_style_path_derives_and_matches() {
        let cfg = listing(&[("transcripts", r"D:\projects\transcripts")]);
        assert_eq!(listed_dir("transcripts", &cfg, None), Ok(PathBuf::from(r"D:\projects\transcripts")));
    }

    /// Absence is not trust. The map records directories Claude Code has been
    /// opened in, so "no entry" is precisely the case that stops at the prompt.
    #[test]
    fn trust_requires_an_explicit_accepted_flag() {
        let cfg: serde_json::Value = serde_json::json!({
            "projects": {
                "/Users/me/Projects/yes": {"hasTrustDialogAccepted": true},
                "/Users/me/Projects/no": {"hasTrustDialogAccepted": false},
                "/Users/me/Projects/silent": {},
            }
        });
        assert!(trusted_in(&cfg, Path::new("/Users/me/Projects/yes")));
        assert!(!trusted_in(&cfg, Path::new("/Users/me/Projects/no")));
        assert!(!trusted_in(&cfg, Path::new("/Users/me/Projects/silent")), "a missing flag is not a yes");
        assert!(!trusted_in(&cfg, Path::new("/Users/me/Projects/absent")), "a directory never opened is not trusted");
        assert!(!trusted_in(&serde_json::json!({}), Path::new("/Users/me/Projects/yes")), "no projects map, no trust");
    }

    #[test]
    fn trust_matching_ignores_separator_style_and_trailing_slashes() {
        let cfg: serde_json::Value = serde_json::json!({"projects": {r"D:\projects\transcripts": {"hasTrustDialogAccepted": true}}});
        assert!(trusted_in(&cfg, Path::new("D:/projects/transcripts")));
        assert!(trusted_in(&cfg, Path::new("D:/projects/transcripts/")));
        assert!(!trusted_in(&cfg, Path::new("D:/projects/Transcripts")), "case is left alone; the ids these paths derive are case-sensitive");
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
    /// slug would make "add me to the list" and "your directory is gone" the
    /// same event to `/investigate`.
    #[test]
    fn every_refusal_has_its_own_slug() {
        let all = [
            StartRefusal::NotListed,
            StartRefusal::PathMismatch,
            StartRefusal::NoSuchDirectory,
            StartRefusal::UntrustedDirectory,
            StartRefusal::AlreadyStarting,
            StartRefusal::NoLauncher,
            StartRefusal::NotRealized,
        ];
        let slugs: std::collections::BTreeSet<&str> = all.iter().map(|r| r.slug()).collect();
        assert_eq!(slugs.len(), all.len());
        assert!(all.iter().all(|r| r.slug().starts_with("start_")), "the prefix is what makes the whole feature greppable in widget.jsonl");
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

    /// The suggestion source is Claude Code's index, so the filtering has to
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
        }});

        let found = candidates_in(&config, "nested", None);
        assert_eq!(found, vec![StartCandidate { dir: sibling_s, trusted: true }]);

        let untrusted = candidates_in(&config, &leaf, None);
        assert_eq!(untrusted, vec![StartCandidate { dir: here_s, trusted: false }], "an untrusted folder is offered, not hidden — the user is looking for it");

        assert!(candidates_in(&config, "no-such-directory-anywhere", None).is_empty(), "a directory that is gone is not a candidate even though the index still lists it");
        assert!(candidates_in(&serde_json::json!({}), "nested", None).is_empty(), "no index, no suggestions");

        let _ = std::fs::remove_dir_all(&here);
    }

    /// The realized check reads a real `agtermctl tree --json` shape — captured
    /// from the live 0.25.0 CLI — so a schema change breaks a test here rather
    /// than silently making every unrealized session look healthy.
    #[cfg(target_os = "macos")]
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

    /// Positive knowledge only. A handle with no id, or a tree we cannot read,
    /// must not report "unrealized" — the caller acts on that by giving up and
    /// closing the session, and doing that to one that is merely slow would
    /// destroy a live agent.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unknown_session_is_never_reported_unrealized() {
        assert!(!LaunchHandle { session_id: None }.is_unrealized());
    }

    /// The command shape is the whole requirement — a session the user can walk
    /// up to — so it is pinned rather than left to drift.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_agterm_command_leaves_a_live_shell_behind() {
        assert!(AGTERM_COMMAND.contains("zsh -ilc"), "-lc resolves no claude from a GUI app's PATH");
        assert!(AGTERM_COMMAND.contains("exec zsh -i"), "without this the session closes when claude exits and there is nothing to walk up to");
        assert!(AGTERM_COMMAND.contains("claude"));
    }
}
