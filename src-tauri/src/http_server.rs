use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use tauri::{AppHandle, Manager};

use crate::adapters::{self, AdapterOutput};
use crate::chat_id_registry::ChatIdRegistry;
use crate::commands::{emit_sessions_updated, now_ms, resolved_snapshot};
use crate::config::ConfigState;
use crate::log_watcher::WatcherRegistry;
use crate::nonce_store::NonceStore;
use crate::peer_message::{self, Outcome, Receipt};
use crate::prompt_history::PromptHistoryStore;
use crate::session_registry::{Activity, LiveSession, SessionRegistry};
use crate::state::{AgentSession, AppState, Status};
use crate::sync::SyncListening;

pub async fn run(app: AppHandle, port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "http bind failed");
            return;
        }
    };
    tracing::info!(%addr, "http listening");

    let router = Router::new()
        .route("/api/event", post(post_event))
        .route("/api/agents", get(get_agents))
        .route("/api/message", post(post_message))
        .with_state(app);

    if let Err(e) = axum::serve(listener, router).await {
        tracing::error!(error = %e, "http serve ended");
    }
}

/// Incoming wire shape for `/api/event`. The hook forwards Claude Code's raw
/// lifecycle payload; `adapters::dispatch` turns it into a
/// `SetInput` / `Clear` / `Ignore` based on `client` + `event`.
#[derive(Deserialize, Debug)]
struct EventRequest {
    client: String,
    event: String,
    #[serde(default)]
    payload: serde_json::Value,
    /// Candidate pids the session's terminal is reachable through — the hook's
    /// console process list plus its ancestor chain (so the long-lived Claude
    /// Code process is included). `terminal_title` uses them to set the terminal
    /// tab title. Sent on both Windows and macOS; absent only from pre-field hooks.
    #[serde(default)]
    console_pids: Vec<u32>,
    /// Pid of the owning Claude Code process (`claude.exe` / `claude`), resolved
    /// by the hook from its ancestor chain and reported fresh on every event.
    /// `liveness_reaper` checks it to remove a row whose session exited without a
    /// `SessionEnd`. `None` when the hook couldn't identify it (e.g. a node-based
    /// install) or from pre-field hooks.
    #[serde(default)]
    agent_pid: Option<u32>,
}

/// Response body for `/api/event`. Empty for most events; on `SessionStart` with
/// the instruction-adherence canary enabled it carries `additional_context` — the
/// text the hook injects as `hookSpecificOutput.additionalContext` so Claude ends
/// every reply with this session's hidden marker.
#[derive(Serialize, Default, Debug)]
struct EventResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_context: Option<String>,
}

/// Decide the canary nonce to inject on a `SessionStart` of the given `source`,
/// or `None` to leave the session untracked (skip injection).
///
/// `startup` (brand-new session) and `clear` (`/clear` wiped the context) leave
/// the model with no prior marker instruction, so mint a fresh nonce. `resume`
/// and `compact` keep the model's prior context — its ORIGINAL marker
/// instruction is still live — so reuse the session's existing nonce; minting
/// there would inject a second, conflicting marker the model won't adopt,
/// permanently mismatching the expected nonce (the "stuck Pending on resume"
/// bug). If a resume has no retained nonce (the app restarted mid-session), the
/// marker the model is already emitting is unknowable, so return `None` rather
/// than mint a conflict — the row reads `Off` until its next fresh start.
fn session_start_nonce(ns: &NonceStore, chat_id: &str, source: &str, now_ms: i64) -> Option<String> {
    if matches!(source, "resume" | "compact") {
        ns.get(chat_id).map(|(nonce, _seen)| nonce)
    } else {
        Some(ns.mint(chat_id, now_ms))
    }
}

/// Whether an end signal may remove the row it names. A cwd-derived chat_id is
/// shared by every Claude Code instance in that directory, so a `SessionEnd`
/// from one can arrive for a row another is still writing — canonically after a
/// `--fork-session --resume` migrates a terminal session into a background one,
/// leaving both resident. The row's owner is the `session_id` of whichever
/// instance wrote it last (`ChatIdRegistry::claim`).
///
/// Ownership is deliberately keyed on the payload's `session_id` rather than
/// the hook's `agent_pid`. The pid is resolved by walking the hook's ancestors
/// for a live `claude` image, so it comes back `None` for a session that is
/// *shutting down* — precisely when this guard has to hold. A real `SessionEnd`
/// from a killed sibling was let through that way; `session_id` is in the
/// payload unconditionally and needs no live process to read.
///
/// Permitted unless both ids are known *and* differ: nothing claimed since a
/// restart, or a payload without a `session_id`, means ownership is unknown and
/// an authoritative end signal beats a guess. `/clear` still removes its own
/// row — it fires `SessionEnd` under the *old* session_id, which is the one
/// that claimed the row, and mints the new id only on the following
/// `SessionStart`.
fn clear_permitted(owner: Option<&str>, ending: &str) -> bool {
    match owner {
        Some(owner) if !ending.is_empty() => owner == ending,
        _ => true,
    }
}

/// What the per-`Stop` canary check should do to the surfaced `instruction_drift`
/// flag. `Clear`/`Confirm` write it; `Hold` leaves it exactly as-is.
#[derive(Debug, PartialEq, Eq)]
enum DriftAction {
    /// Marker present — the agent is adhering; clear any prior drift.
    Clear,
    /// Marker dropped on a *completion* turn after prior adherence — surface drift.
    Confirm,
    /// Don't touch the flag: either an unconfirmed session (`!seen`, instruction may
    /// be undelivered), or a dropped marker on a *handback* turn we defer to the
    /// next completion turn so a mid-workflow skill turn can't false-alarm.
    Hold,
}

/// Decide the canary action from the settled turn's shape. A dropped marker only
/// *confirms* drift on a completion turn; on a `Blocked` handback (`is_handback`)
/// the drop is deferred (`Hold`) and re-judged next turn — the model mid-workflow
/// (e.g. a `/commit` reflection ending on a question) legitimately drops the hidden
/// marker and picks it back up once the workflow ends, so confirming there would be
/// a false alarm. `seen` gates everything: an unconfirmed session is always held.
fn drift_action(present: bool, seen: bool, is_handback: bool) -> (DriftAction, &'static str) {
    if present {
        (DriftAction::Clear, "adherence marker present")
    } else if !seen {
        (DriftAction::Hold, "marker absent but never confirmed (instruction may be undelivered); holding")
    } else if is_handback {
        (DriftAction::Hold, "handback turn dropped the marker; deferring to the next completion turn")
    } else {
        (DriftAction::Confirm, "completion turn dropped the marker after prior adherence")
    }
}

/// CSRF guard for every route on this server: block browser-originated requests.
/// urllib / curl don't send `Origin`; browser XHRs do. `"null"` is allowed
/// (`file://` / `data:`). The server is loopback-only and unauthenticated, so a
/// page the user happens to have open is the whole threat model.
///
/// **This is CSRF only, and does not close DNS rebinding.** An attacker page
/// whose domain is rebound to `127.0.0.1` becomes same-origin with this server,
/// so the browser sends no `Origin` at all and the request is allowed. That was
/// close to worthless when the only route was a write; `GET /api/agents` makes it
/// a read of every project name, status and label. Closing it means also
/// requiring a loopback `Host`, which is deliberately *not* done here: the hook's
/// target is overridable via `TAURI_DASHBOARD_URL`, so a host alias that resolves
/// to loopback is a supported setup that such a check would break. Recorded
/// rather than papered over — the gap is a known, accepted one.
///
/// **Reassessed for `POST /api/message`, and priced differently there.** That
/// gap was accepted while every route on this server wrote a status or read a
/// roster. The message route starts a turn inside a live agent *on another
/// machine*, so the same rebound page would become a way to prompt a remote
/// agent — a different order of consequence. It therefore adds
/// [`host_is_loopback`] on top of this check. The hook routes keep the accepted
/// gap, because the `TAURI_DASHBOARD_URL` alias they support is real and the
/// stake there is unchanged.
///
/// Extracted rather than copied into the other handlers: the guard is
/// per-handler (there is no tower layer on this router), so every new route is
/// unprotected until it repeats the check, and two copies of the rule would be
/// two places to forget when it changes.
fn origin_blocked(headers: &HeaderMap) -> bool {
    match headers.get("origin") {
        None => false,
        Some(origin) => !matches!(origin.to_str(), Ok("null")),
    }
}

/// Whether the request addressed us by a loopback name — the second gate on the
/// message route, and the half that closes DNS rebinding for it.
///
/// A rebound page reaches this server with the attacker's own hostname in
/// `Host`, because that is the name the browser resolved; a genuine local caller
/// has no reason to use anything but loopback. `localhost` is accepted because
/// the default hook URL and every hand-typed `curl` use it. A missing `Host`
/// is rejected: HTTP/1.1 requires it, so its absence is not a client we support.
fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host.trim();
    // Strip the port, honouring the bracketed form an IPv6 literal must use.
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Body of `GET /api/agents` — the roster an agent reads to answer "does a
/// session for project X exist, on which machine, in what state, and how fresh
/// is that answer?". Claude Code's own session listing sees only the local
/// machine; this dashboard already merges local + synced-from-peer rows, so it
/// can answer across both.
///
/// `peers` is not redundant with the rows: it is the only place a *device* with
/// zero sessions can appear, which is what separates "the other machine is up
/// and has nothing for project X" from "the other machine has said nothing in
/// 80 s". Both arrays are always present and never null.
#[derive(Serialize, Debug, PartialEq)]
struct AgentsResponse {
    /// The machine being queried, so one merged roster is self-describing.
    /// `null` — never a sentinel — when `sync.device_name` was never
    /// bootstrapped; inventing a `"local"` could collide with a real peer name.
    device: Option<String>,
    /// Whether this dashboard can receive peer rows at all
    /// (`sync.listen` + a token). When false, an empty `peers` says *nothing*
    /// about the other machine and the caller must not read it as "no sessions
    /// there".
    sync_listening: bool,
    peers: Vec<PeerRow>,
    agents: Vec<AgentRow>,
    /// Live local sessions Claude Code's own registry knows about and the hook
    /// stream does not — see `RegistryRow`. A project present in both arrays
    /// appears only in `agents`, so the name states the precedence rule.
    registry_only: Option<Vec<RegistryRow>>,
}

/// One synced peer dashboard. `sessions` counts the rows attributed to it in
/// `agents`, so a device that is live but idle is visible as itself.
#[derive(Serialize, Debug, PartialEq)]
struct PeerRow {
    device: String,
    last_seen_age_ms: i64,
    sessions: usize,
}

/// One tracked session, local or synced.
///
/// Deliberately absent, and not to be re-added: **no `deliverable` / `sendable`
/// boolean.** A remote row's status can be up to a reap window old, so a green
/// light computed here would state as fact something derived from possibly-stale
/// data — this returns the facts and the age of each, and lets the caller judge.
/// **No user-presence / idle-ms either**: a message to a peer agent starts a turn
/// in it whether or not a human is watching that screen, so presence changes no
/// decision the caller makes. Also omitted for size and for the trust boundary:
/// the dialog, the canary/drift flags, tokens and model.
#[derive(Serialize, Debug, PartialEq)]
struct AgentRow {
    /// Dashboard-canonical id, namespaced exactly as everything else in this
    /// repo addresses a row (`chrome/transcripts`).
    id: String,
    /// The de-namespaced cwd-derived id (equal to `id` for a local row). This is
    /// the cross-machine comparable key: `derive_chat_id` normalizes backslashes
    /// and strips the projects root, so one project yields the same string on
    /// macOS and on Windows — which is what makes "does project X exist anywhere"
    /// answerable from this body alone.
    project: String,
    /// Which machine it runs on. `null` only when this box has no device name;
    /// kept separate from `local` for exactly that case.
    device: Option<String>,
    local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    status: Status,
    /// What the dashboard row itself shows — the "what is it doing" line. Withheld
    /// it would force the caller to guess from `status` alone, and it is already on
    /// the sync wire; this route is loopback-only, the same trust boundary.
    label: String,
    /// Time in the current `status`. An *age*, not a timestamp, everywhere in this
    /// body: a remote row's clocks are the sender's, so no absolute stamp here would
    /// mean anything without clock agreement, while every question the caller has is
    /// "how old is this". For a remote row this age still carries the sender/receiver
    /// skew (it is derived from the sender's `state_entered_at`); it is clamped at 0
    /// so skew can never read as negative, and `last_seen_age_ms` — which is
    /// skew-free — bounds how much of it to trust.
    status_age_ms: i64,
    /// How long since this row's device last pushed, on the receiver's clock at both
    /// ends, so it is free of clock skew. **This is the field that makes a remote
    /// reading judgeable**: a peer that slept keeps its last-pushed status frozen
    /// until the TTL reaper drops it a heartbeat later, so a bare `"idle"` is
    /// otherwise indistinguishable from a dead machine's last words. Omitted for a
    /// local row, where a `0` would claim freshness on a channel that doesn't exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_age_ms: Option<i64>,
}

/// One live local session the dashboard has heard nothing from this run, read
/// from Claude Code's own session registry (`session_registry::LiveSession`).
///
/// It is a **second array rather than a flag on `agents`** because the two kinds
/// of row answer different questions. An `agents` row means "the dashboard has
/// classified this session and can say what it is doing"; a `registry_only` row
/// means "a live interactive session exists at this project, and the dashboard
/// has heard nothing from it" — enough to answer *does it exist, and where*,
/// never enough to answer *what state*. A `provenance: "hooks" | "registry"`
/// discriminator on one array was rejected twice over: its only job would be to
/// say which meaning `status` and `label` currently hold, which is the "field
/// doing two jobs" anti-pattern, and array membership already distinguishes
/// every case. Making `label` and `status_age_ms` optional instead was rejected
/// too — both are documented as always present, so a caller doing
/// `row.label.toLowerCase()` would break on a row it has no vocabulary for,
/// while a separate array cannot reach that caller at all.
///
/// Local by construction: the registry describes this machine, so there is no
/// `local` field (a constant true is not information) and no `last_seen_age_ms`
/// (there is no push channel behind it). Remote rows keep coming from sync and
/// are untouched by this array.
#[derive(Serialize, Debug, PartialEq)]
struct RegistryRow {
    /// Same cwd-derivation as `AgentRow::id`, which is what makes the union
    /// dedupe by plain equality.
    id: String,
    /// Equal to `id` here — a local id is never namespaced — and present anyway
    /// so a caller uses one pair of keys across both arrays without a special
    /// case for this one.
    project: String,
    device: Option<String>,
    /// Claude Code's own name for the session. Distinct in provenance from
    /// `AgentRow::display_name`, which comes from this dashboard's rename store.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// `idle` / `busy` / `unknown`, the registry's own words. **Never a
    /// `status`**: idle/busy cannot express `blocked`, `waiting` or `error`, so
    /// mapping `busy -> working` would not be coarse but false — a session
    /// parked on a question is `blocked` here and `idle` there.
    activity: Activity,
    /// Time since the registry last wrote that activity. Omitted when the
    /// record carries no stamp; skew-free, since the registry is local.
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_age_ms: Option<i64>,
    /// How many interactive sessions collapsed into this row (2 after a fork
    /// migration left two tabs in one directory).
    sessions: usize,
}

/// Shape the merged roster. All the judgment lives here so it is testable without
/// a router or an `AppHandle`; the handler is a thin assembler.
///
/// A remote row's device prefix is stripped by its `origin` field, never by
/// splitting on the first `/` — a device name may itself contain a slash, and
/// `sync::resolve_fetch_target` already matches whole names for the same reason.
/// A remote row whose `origin` is missing from `last_seen` is dropped rather than
/// reported: the sessions and the device map are read under two separate locks, so
/// a device reaped between them leaves a phantom row, and a row with no freshness
/// number is precisely the thing this route exists to never emit.
///
/// `registry` is the second session source: Claude Code's own list of live local
/// sessions, which unlike the hook stream survives a dashboard restart. It is
/// merged *here* rather than upstream of the call because the three things this
/// union actually decides — dedupe by id, hook-derived wins, and the collapse of
/// two interactive sessions in one cwd — are exactly what wants to be testable
/// with plain data. Pre-merging would mean fabricating a `label`,
/// `state_entered_at` and `status` for a row we know none of, and setting
/// `origin: None` on it, which is the authoritative local test both this function
/// and `PeerRow::sessions` key on; the fabricated row would then be
/// indistinguishable from a hook row right where the precedence rule has to run.
fn agent_roster(
    sessions: &[AgentSession],
    registry: Option<&[LiveSession]>,
    anchored: &dyn Fn(&str) -> Option<String>,
    last_seen: &BTreeMap<String, i64>,
    this_device: Option<&str>,
    sync_listening: bool,
    now_ms: i64,
) -> AgentsResponse {
    let age_since = |then: i64| (now_ms - then).max(0);

    let agents: Vec<AgentRow> = sessions
        .iter()
        .filter_map(|s| {
            let (device, project, seen) = match s.origin.as_deref() {
                None => (this_device.map(str::to_string), s.id.clone(), None),
                Some(origin) => {
                    let seen = last_seen.get(origin)?;
                    let project = s.id.strip_prefix(&format!("{origin}/")).unwrap_or(&s.id).to_string();
                    (Some(origin.to_string()), project, Some(age_since(*seen)))
                }
            };
            Some(AgentRow {
                id: s.id.clone(),
                project,
                device,
                local: s.origin.is_none(),
                display_name: s.display_name.clone(),
                status: s.status,
                label: s.label.clone(),
                status_age_ms: age_since(s.state_entered_at),
                last_seen_age_ms: seen,
            })
        })
        .collect();

    // Hook-derived wins every conflict: a hook row carries a real dashboard
    // status, a label and a task history, all of which the registry's coarse
    // idle/busy would only blur. Matched against *local* ids only — a registry
    // cwd can legitimately derive the same `project` as a remote row (that is
    // what `project` is for), and deduping across machines would delete the
    // remote row's evidence that the project also runs over there.
    //
    // The id compared here is the *anchored* one where the session has been seen
    // before. A hook row's id is pinned by `ChatIdRegistry` at first sight and
    // never re-derived, so it survives a mid-session `cd`; the registry record
    // carries the session's current cwd, which after any `cd` derives a
    // different id. Comparing the derivation against the anchor would then miss,
    // and one live session would appear in both arrays under two ids with
    // nothing marking them as the same session — handing a caller a project that
    // does not exist. `anchored` is a read-only lookup: it inserts nothing, so
    // this stays a read path.
    let hook_local: HashSet<&str> = agents.iter().filter(|a| a.local).map(|a| a.id.as_str()).collect();
    let registry_only: Option<Vec<RegistryRow>> = registry.map(|regs| regs
        .iter()
        .map(|s| (s, s.session_ids.iter().find_map(|sid| anchored(sid)).unwrap_or_else(|| s.chat_id.clone())))
        .filter(|(_, id)| !hook_local.contains(id.as_str()))
        .map(|(s, id)| RegistryRow {
            project: id.clone(),
            id,
            device: this_device.map(str::to_string),
            name: s.name.clone(),
            activity: s.activity,
            activity_age_ms: s.activity_age_ms,
            sessions: s.sessions,
        })
        .collect());

    let peers = last_seen
        .iter()
        .map(|(device, seen)| PeerRow {
            device: device.clone(),
            last_seen_age_ms: age_since(*seen),
            // `!a.local` matters: a local row carries this device's own name, so
            // matching on the name alone folds every local session into a peer's
            // count whenever the two machines share a name — which they do by
            // default, since `device_name` bootstraps from the hostname, and in
            // the repo's own localhost-observer sync test setup.
            sessions: agents.iter().filter(|a| !a.local && a.device.as_deref() == Some(device.as_str())).count(),
        })
        .collect();

    AgentsResponse { device: this_device.map(str::to_string), sync_listening, peers, agents, registry_only }
}

/// Read-only roster of every session this dashboard tracks, local and synced.
/// Mutates nothing — no `apply_set`, no emit, no `SyncDirty` poke, no store
/// write — which is also why it writes no `decision` line: every one of those
/// tags marks a state change and the `investigate` skill replays them to
/// reconstruct a row, so a polling reader would bury the real decisions under
/// entries that explain no state.
async fn get_agents(
    State(app): State<AppHandle>,
    headers: HeaderMap,
) -> Result<Json<AgentsResponse>, StatusCode> {
    if origin_blocked(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(state) = app.try_state::<AppState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let cfg = cfg_state.snapshot();
    let now = now_ms();
    let sessions = resolved_snapshot(&app);
    let last_seen = state.remote_last_seen();
    // The second session source. Reading it can fork a `ps` for the process-table
    // snapshot, which is blocking IO on a tokio worker — accepted rather than
    // hidden: it is shared with `terminal_title::sync` behind the registry's own
    // 5 s cache, so it runs at most once per 5 s however fast this route is
    // polled, and `post_event` on the same server already does blocking IO.
    let registry = app
        .try_state::<SessionRegistry>()
        .and_then(|r| r.live_sessions(cfg.projects_root.as_deref(), now));
    let this_device = Some(cfg.sync.device_name.trim()).filter(|d| !d.is_empty());
    // Read the running listener, never the config predicate that was supposed to
    // produce it: an empty-string token, a hot-reloaded `sync.listen`, and a
    // failed bind each make config claim a listener that isn't there. See
    // `sync::SyncListening`.
    let sync_listening = app.try_state::<SyncListening>().is_some_and(|f| f.get());
    let chat_ids = app.try_state::<ChatIdRegistry>();
    Ok(Json(agent_roster(&sessions, registry.as_deref(), &|sid| chat_ids.as_ref().and_then(|r| r.anchored(sid)), &last_seen, this_device, sync_listening, now)))
}

/// Body of `POST /api/message` — a local agent asking this dashboard to relay a
/// message to an agent on another machine.
#[derive(Deserialize, Debug)]
struct MessageRequest {
    /// A `{device}/{project}` address, echoed from `/api/agents`. A bare project
    /// name is refused rather than guessed at; see `resolve_message_target`.
    target: String,
    text: String,
    /// The caller's own chat_id. A **claim** — this server is loopback and
    /// unauthenticated, so nothing here is checked — carried so the receiving
    /// model is told who says it sent this, and so each originating agent gets
    /// its own admission bucket on the receiver.
    #[serde(default)]
    from_agent: Option<String>,
    #[serde(default)]
    from_label: Option<String>,
}

/// Relay one message to an agent on another machine.
///
/// **This is the only entry point that holds the originating agent's identity**,
/// so it is where the message id is minted and where the claim is attached; by
/// the time the frame reaches a socket, the writing process is a dashboard and
/// the claim is all that is left of the sender.
///
/// It refuses exactly the two things it can know for certain and no more. A
/// local target is certain (an exact local id, or this device's own name in the
/// device half) and is refused with `SendMessage` named, because that tool
/// carries a kernel-verified sender and a reply address this route destroys. An
/// unheard-of device is certain (we hold no address for it at all). Everything
/// else — above all *does that project exist over there* — is the receiving
/// dashboard's answer, returned as a receipt outcome, because this side's roster
/// is at best one push cycle old and asserting an existence from it would be
/// stating a stale reading as fact.
async fn post_message(State(app): State<AppHandle>, headers: HeaderMap, Json(req): Json<MessageRequest>) -> (StatusCode, Json<Receipt>) {
    let from_agent = req.from_agent.as_deref().map(str::trim).filter(|a| !a.is_empty()).unwrap_or("unknown");
    // Every exit from this handler goes through here, so no refusal can be the
    // one that leaves no trace — including the two that happen before the
    // dashboard's own state is even reachable.
    let refused = |reason: &str, detail: String, status: StatusCode, device: Option<&str>| {
        let r = Receipt::new(Outcome::Refused, "", &req.target, device).because(reason).detailed(detail);
        log_send(&r, from_agent, &req.target, device, req.text.len());
        (status, Json(r))
    };
    // CSRF, then the stricter rebinding check this route alone carries.
    if origin_blocked(&headers) || !host_is_loopback(&headers) {
        return refused("csrf", "this route answers only a loopback caller addressing it by a loopback name".into(), StatusCode::FORBIDDEN, None);
    }
    if req.text.trim().is_empty() {
        return refused("empty_text", "there is nothing to relay".into(), StatusCode::BAD_REQUEST, None);
    }
    if req.text.len() > peer_message::MAX_TEXT_BYTES {
        return refused("too_large", format!("{} bytes, cap is {}", req.text.len(), peer_message::MAX_TEXT_BYTES), StatusCode::PAYLOAD_TOO_LARGE, None);
    }
    let (Some(state), Some(cfg_state), Some(ids)) = (app.try_state::<AppState>(), app.try_state::<ConfigState>(), app.try_state::<peer_message::MessageIds>()) else {
        return refused("state_unavailable", "the dashboard is still starting".into(), StatusCode::INTERNAL_SERVER_ERROR, None);
    };
    let cfg = cfg_state.snapshot();
    let now = now_ms();
    let this_device = Some(cfg.sync.device_name.trim()).filter(|d| !d.is_empty());

    // Local ids come from both session sources, so a session the hook stream has
    // not heard from this run is still recognized as local rather than being
    // relayed to a machine it is not on.
    let mut local_ids: Vec<String> = resolved_snapshot(&app).into_iter().filter(|s| s.origin.is_none()).map(|s| s.id).collect();
    if let Some(registry) = app.try_state::<SessionRegistry>() {
        if let Some(live) = registry.live_sessions(cfg.projects_root.as_deref(), now) {
            local_ids.extend(live.into_iter().map(|s| s.chat_id));
        }
    }
    let last_seen = state.remote_last_seen();
    let devices: Vec<String> = last_seen.keys().cloned().collect();

    let (device, project) = match peer_message::resolve_message_target(&req.target, &local_ids, &devices, this_device) {
        peer_message::TargetResolution::Remote { device, project } => (device, project),
        peer_message::TargetResolution::Local => {
            return refused(
                "local_target",
                "this session is on this machine; use Claude Code's `SendMessage`, which carries a kernel-verified sender identity and a reply address this route destroys".into(),
                StatusCode::BAD_REQUEST,
                this_device,
            );
        }
        peer_message::TargetResolution::UnknownDevice { device } => {
            // Naming what we do know, plus whether we can hear a peer at all: an
            // empty device list under a listener that never bound says nothing
            // about the other machine, which is the same distinction
            // `AgentsResponse.sync_listening` exists to make.
            let sync_listening = app.try_state::<SyncListening>().is_some_and(|f| f.get());
            let known = if devices.is_empty() { "none".to_string() } else { devices.join(", ") };
            return refused(
                "unknown_device",
                format!("no address for device \"{device}\"; devices heard from: {known} (sync_listening={sync_listening})"),
                StatusCode::NOT_FOUND,
                None,
            );
        }
        peer_message::TargetResolution::NotAnAddress => {
            return refused(
                "not_an_address",
                format!("expected a \"{{device}}/{{project}}\" address from /api/agents; devices heard from: {}", if devices.is_empty() { "none".to_string() } else { devices.join(", ") }),
                StatusCode::BAD_REQUEST,
                None,
            );
        }
    };

    // A device in the roster always has an address, but it can age out between
    // the two reads, and `origin_addr` is only ever populated by an inbound
    // push — a peer we push to but have never heard from has none.
    let origin_addr = state.remote.lock().unwrap().get(&device).map(|d| d.origin_addr.clone()).unwrap_or_default();
    if origin_addr.is_empty() {
        let age = last_seen.get(&device).map(|s| (now - s).max(0));
        return refused(
            "device_unheard",
            format!("no address for \"{device}\" yet — it has not pushed to this device (last_seen_age_ms={age:?})"),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(&device),
        );
    }

    // A machine that cannot name itself cannot be deduped against or attributed
    // to, so the peer would refuse the envelope one hop later with a vaguer
    // reason. Refuse here, where the fix (set `sync.device_name`) is local.
    let Some(this_device) = this_device else {
        return refused(
            "no_device_name",
            "this machine has no sync.device_name, so a relayed message could not be attributed or deduplicated".into(),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(&device),
        );
    };

    let envelope = crate::sync::MessageEnvelope {
        origin_device: this_device.to_string(),
        message_id: peer_message::mint_message_id(this_device, now, ids.next()),
        target_project: project,
        from_agent: from_agent.to_string(),
        from_label: req.from_label.clone(),
        text: req.text.clone(),
    };
    let receipt = crate::sync::send_message_hop(&app, &origin_addr, &envelope, &req.target, &device).await;
    log_send(&receipt, from_agent, &req.target, Some(&device), req.text.len());
    (crate::sync::receipt_status(&receipt), Json(receipt))
}

/// The permanent record of one relay attempt, keyed by the **sender's** chat_id
/// so `/investigate` can reach it (`agent_of` resolves an entry by `chat_id`),
/// with the target as its own field. Unlike `/api/agents`, which mutates nothing
/// and so writes no `decision` line, a send changes state on another machine.
///
/// A refusal is tagged `peer_refused` at `warn` rather than folded into
/// `peer_send`, so "it would not send" is greppable on its own — the states this
/// route refuses (a target on the wrong machine, a device that never pushed) are
/// misconfigurations only the user can fix, the same reasoning `sync::log_reject`
/// follows.
///
/// The message body never appears here, in any branch.
fn log_send(receipt: &Receipt, from_agent: &str, target: &str, device: Option<&str>, text_len: usize) {
    let refused = receipt.outcome == Outcome::Refused;
    let decision = if refused { "peer_refused" } else { "peer_send" };
    macro_rules! line {
        ($level:ident) => {
            tracing::$level!(
                chat_id = %from_agent,
                decision,
                target = %target,
                device = ?device,
                message_id = %receipt.message_id,
                text_len,
                outcome = ?receipt.outcome,
                reason = ?receipt.reason,
                "relay attempt"
            )
        };
    }
    if refused {
        line!(warn);
    } else {
        line!(info);
    }
}

async fn post_event(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(req): Json<EventRequest>,
) -> (StatusCode, Json<EventResponse>) {
    if origin_blocked(&headers) {
        return (StatusCode::FORBIDDEN, Json(EventResponse::default()));
    }

    let Some(state) = app.try_state::<AppState>() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default()));
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default()));
    };
    let cfg = cfg_state.snapshot();
    let mut resp = EventResponse::default();

    let mut output = adapters::dispatch(&req.client, &req.event, &req.payload, &cfg);

    // Lock the row to the Claude session_id so a mid-session cwd change (the
    // agent `cd`s into a subdirectory) doesn't fragment one conversation across
    // multiple rows. `/clear` mints a new session_id with the same cwd, so it
    // re-derives the same id and the row stays continuous.
    let session_id = req.payload.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(registry) = app.try_state::<ChatIdRegistry>() {
        match &mut output {
            AdapterOutput::Set { input, .. } => {
                input.id = registry.resolve(session_id, &input.id);
            }
            AdapterOutput::Clear { id } => {
                *id = registry.resolve(session_id, id);
                registry.forget(session_id);
            }
            AdapterOutput::Boundary { id } => {
                *id = registry.resolve(session_id, id);
            }
            AdapterOutput::Ignore => {}
        }
    }

    match output {
        AdapterOutput::Set { input, transcript_path, reason } => {
            // Permanent decision record: why this row landed in this state. The
            // `decision` field makes it greppable (the `investigate` skill reads
            // these), and `reason` carries the matched question-rule + a text
            // snippet so "why is it Blocked?" is answerable without the
            // transcript or the code. Keyed by the resolved chat_id.
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %input.id,
                decision = "classify",
                status = ?input.status,
                label = ?input.label,
                reason = %reason,
                console_pids = ?req.console_pids,
                agent_pid = ?req.agent_pid,
                "event -> set"
            );
            let chat_id = input.id.clone();
            // Remember which console hosts this session so terminal_title can
            // push tab-title updates. Cleanup is centralized in
            // `terminal_title::sync` — when the session row disappears (Clear,
            // manual removal) the title is blanked and the pids forgotten.
            if let Some(titles) = app.try_state::<crate::terminal_title::TerminalTitles>() {
                titles.register(&chat_id, &req.console_pids);
            }
            // Record the owning Claude pid so `liveness_reaper` can detect a
            // session that exits without a SessionEnd. Overwrite each event so a
            // same-cwd restart's new pid supersedes a now-dead one.
            if let Some(pid) = req.agent_pid {
                if let Some(pids) = app.try_state::<crate::liveness::AgentPids>() {
                    pids.set(&chat_id, pid);
                }
            }
            // Claim the row for this session, so a `SessionEnd` from another
            // instance sharing the cwd can't remove it (see `clear_permitted`).
            if let Some(registry) = app.try_state::<ChatIdRegistry>() {
                registry.claim(&chat_id, session_id);
            }
            let history = app.try_state::<PromptHistoryStore>();
            let restored = history.as_ref().and_then(|h| h.get(&chat_id));
            let now = now_ms();
            let watcher = app.try_state::<WatcherRegistry>();
            let set_changed = state.apply_set(input, now, &cfg.continuation_prompts, restored);
            if set_changed {
                if let Some(ref h) = history {
                    let sessions = state.sessions.lock().unwrap();
                    if let Some(s) = sessions.iter().find(|s| s.id == chat_id) {
                        h.save_session(s);
                    }
                    drop(sessions);
                    h.save_to_disk();
                }
            }
            // --- Instruction-adherence canary (see Config::instruction_canary_enabled) ---
            // On SessionStart, mint (startup/clear) or reuse (resume/compact) the
            // session's nonce and hand the hook the instruction to inject; on Stop,
            // a dropped marker on the settled turn's final message flags orthogonal
            // drift (status is untouched).
            if cfg.instruction_canary_enabled {
                if req.event == "SessionStart" {
                    if let Some(ns) = app.try_state::<crate::nonce_store::NonceStore>() {
                        // A `resume`/`compact` keeps the model's prior context (and
                        // its original marker), so reuse the existing nonce rather
                        // than mint a second, conflicting one; only `startup`/`clear`
                        // (no prior marker in context) rotate. See `session_start_nonce`.
                        let source = req.payload.get("source").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(nonce) = session_start_nonce(&ns, &chat_id, source, now) {
                            let marker = crate::adapters::claude::marker_for(crate::adapters::claude::CANARY_MARKER, &nonce);
                            resp.additional_context = Some(format!(
                                "Adherence check for this session: end every response you write with the exact text {marker}, \
                                 placed inline on the same line right after your final character (a single space before it, \
                                 no blank line) — a hidden marker, so do not mention, explain, or alter it."
                            ));
                        }
                    }
                } else if req.event == "Stop" {
                    // Judged only when this session has a nonce and produced a final
                    // message; a tool-only / empty-final turn is exempt (left as-is).
                    let final_msg = req.payload.get("last_assistant_message").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
                    if let (Some(final_msg), Some(ns)) = (final_msg, app.try_state::<crate::nonce_store::NonceStore>()) {
                        if let Some((nonce, seen)) = ns.get(&chat_id) {
                            let marker = crate::adapters::claude::marker_for(crate::adapters::claude::CANARY_MARKER, &nonce);
                            let present = final_msg.contains(&marker);
                            if present {
                                ns.mark_seen(&chat_id);
                            }
                            // "starts to skip": flag drift only once the session has
                            // PROVEN it can emit the marker (`seen`). An unconfirmed
                            // session — e.g. one whose SessionStart response was lost, so
                            // the marker instruction never reached the model — is held
                            // unflagged, so a delivery miss can't manufacture a permanent
                            // false drift; only a drop *after* prior adherence flags.
                            // Two-tier: a drop on a `Blocked` handback (the model mid-
                            // workflow — e.g. a `/commit` reflection ending on a question)
                            // is deferred, not confirmed, and re-judged next turn (see
                            // `drift_action`), so a self-correcting skill turn never pings.
                            let is_handback = state.status_of(&chat_id) == Some(crate::state::Status::Blocked);
                            let (action, reason) = drift_action(present, seen, is_handback);
                            let changed = match action {
                                DriftAction::Clear => state.set_drift(&chat_id, false, now),
                                DriftAction::Confirm => state.set_drift(&chat_id, true, now),
                                DriftAction::Hold => false,
                            };
                            let drifted = state.drift_confirmed(&chat_id);
                            let deferred = matches!(action, DriftAction::Hold) && seen && !present;
                            tracing::debug!(chat_id = %chat_id, decision = "drift_check", drifted, deferred, seen, changed, marker = %marker, reason, "canary drift check");
                        }
                    }
                }
            }
            if let Some(tp) = transcript_path {
                if let Some(reg) = watcher {
                    reg.start(app.clone(), chat_id, tp);
                }
            }
            emit_sessions_updated(&app);
        }
        AdapterOutput::Clear { id } => {
            // Two Claude Code instances can hold one cwd — canonically a terminal
            // session forked into a background/desktop one (`--fork-session
            // --resume`) — and the chat_id is cwd-derived, so both address the
            // same row. Whoever wrote last owns it: refuse an end signal from the
            // *other* instance, which would otherwise flush and drop a row a live
            // sibling is still using.
            let owner = app.try_state::<ChatIdRegistry>().and_then(|r| r.owner_of(&id));
            if !clear_permitted(owner.as_deref(), session_id) {
                tracing::debug!(
                    client = %req.client,
                    event = %req.event,
                    chat_id = %id,
                    decision = "clear_ignored",
                    reason = "end signal from a non-owning session sharing this cwd",
                    owner = ?owner,
                    ending = %session_id,
                    "event -> clear"
                );
                return (StatusCode::OK, Json(resp));
            }
            if let Some(registry) = app.try_state::<ChatIdRegistry>() {
                registry.disown(&id);
            }
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %id,
                decision = "session_clear",
                reason = "session ended; row removed",
                "event -> clear"
            );
            // Remove the row through the shared helper — the same path the
            // liveness reaper uses, so the two can't drift. It appends a history
            // separator before dropping the in-memory session: Claude `/clear`
            // fires SessionEnd → SessionStart, so persisting a dialog that ends
            // with the separator lets the next SessionStart's "new" branch
            // restore it and land the upcoming UserPromptSubmit after the
            // boundary. `None` = remove unconditionally (this is the
            // authoritative end signal, not a speculative reap).
            crate::commands::remove_session(&app, &id, None, now_ms());
            // Drop the session's canary nonce only on a `/clear`, which wipes the
            // model's context (and its marker instruction); the next
            // SessionStart:clear then mints a fresh one. A plain exit/logout keeps
            // the nonce: the session may be resumed with its context (and original
            // marker) intact, and `session_start_nonce` reuses it so a resumed
            // session stays confirmed instead of falsely rotating to a marker the
            // model isn't emitting.
            if req.payload.get("reason").and_then(|v| v.as_str()) == Some("clear") {
                if let Some(ns) = app.try_state::<crate::nonce_store::NonceStore>() {
                    ns.forget(&id);
                }
            }
        }
        AdapterOutput::Boundary { id } => {
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %id,
                decision = "compact_boundary",
                reason = "context compaction; history separator inserted",
                "event -> boundary"
            );
            // The session continues (no status change) — just append a history
            // separator marking the context boundary. Idempotent, so a parallel
            // transcript-rotation marking the same boundary is harmless.
            let now = now_ms();
            if state.mark_session_boundary(&id, now) {
                if let Some(h) = app.try_state::<PromptHistoryStore>() {
                    let sessions = state.sessions.lock().unwrap();
                    if let Some(s) = sessions.iter().find(|s| s.id == id) {
                        h.save_session(s);
                    }
                    drop(sessions);
                    h.save_to_disk();
                }
                emit_sessions_updated(&app);
            }
        }
        AdapterOutput::Ignore => {
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                "event -> ignored"
            );
        }
    }
    (StatusCode::OK, Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: Status, origin: Option<&str>, state_entered_at: i64) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status,
            status_before_working: Status::Idle,
            label: "label".into(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: origin.map(str::to_string),
            instruction_drift: false,
            canary: crate::state::Canary::Off,
        }
    }

    fn devices(entries: &[(&str, i64)]) -> BTreeMap<String, i64> {
        entries.iter().map(|(d, seen)| (d.to_string(), *seen)).collect()
    }

    #[test]
    fn a_local_row_reports_no_last_seen_age() {
        // There is no sync channel behind a local row, so a `0` there would claim a
        // freshness that means nothing. The field is absent instead.
        let rows = agent_roster(&[session("transcripts", Status::Working, None, 900)], Some(&[]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        let row = &rows.agents[0];
        assert!(row.local, "origin.is_none() is the authoritative local test");
        assert_eq!(row.last_seen_age_ms, None, "a local row has no device push to age against");
        assert_eq!(row.device.as_deref(), Some("air"), "a local row is attributed to this machine");
        assert_eq!(row.status_age_ms, 100);
    }

    #[test]
    fn a_remote_row_ages_against_its_devices_last_seen() {
        // The freshness number comes from the *device's* last push, not from
        // anything on the row — the row's own stamps are on the sender's clock.
        let sessions = [session("chrome/transcripts", Status::Blocked, Some("chrome"), 763_500)];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("chrome", 995_880)]), Some("air"), true, 1_000_000);
        let row = &rows.agents[0];
        assert_eq!(row.last_seen_age_ms, Some(4_120));
        assert!(!row.local);
        assert_eq!(row.status_age_ms, 236_500);
    }

    #[test]
    fn a_stale_remote_row_is_still_listed_with_its_age() {
        // Past the 90 s TTL but not yet reaped (the reaper ticks on the heartbeat
        // period, so the drop lands 90-120 s after the last push). Report it with
        // its age; hiding it, or turning the age into a verdict, is the caller's
        // call to make and not ours.
        let sessions = [session("chrome/transcripts", Status::Idle, Some("chrome"), 0)];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("chrome", 870_000)]), Some("air"), true, 1_000_000);
        assert_eq!(rows.agents.len(), 1, "a stale row is still a fact about the roster");
        assert_eq!(rows.agents[0].last_seen_age_ms, Some(130_000));
    }

    #[test]
    fn a_remote_row_whose_device_was_reaped_is_dropped() {
        // The two-lock race: the session list and the device map are read
        // separately, so a reap in between leaves a row with no freshness number.
        // Emitting it would be exactly the unjudgeable "idle" this route exists to
        // avoid.
        let sessions = [
            session("transcripts", Status::Working, None, 0),
            session("chrome/transcripts", Status::Idle, Some("chrome"), 0),
        ];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[]), Some("air"), true, 1_000);
        assert_eq!(rows.agents.len(), 1, "the phantom remote row is dropped, the local one stays");
        assert!(rows.agents[0].local);
    }

    #[test]
    fn project_strips_the_device_prefix_by_origin_not_by_slash() {
        // A device name may contain a slash, so splitting on the first one would
        // hand back a project of "box/transcripts" and break the cross-machine
        // comparison this field exists for.
        let sessions = [
            session("win/box/transcripts", Status::Done, Some("win/box"), 0),
            session("tauri dashboard", Status::Working, None, 0),
        ];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("win/box", 1_000)]), Some("air"), true, 1_000);
        assert_eq!(rows.agents[0].project, "transcripts");
        assert_eq!(rows.agents[1].project, "tauri dashboard", "a local id is already de-namespaced");
    }

    #[test]
    fn sender_clock_skew_cannot_produce_a_negative_age() {
        // A remote row's `state_entered_at` is the sender's clock; a fast peer clock
        // puts it in our future. Clamp rather than emit a negative age.
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 5_000)];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("chrome", 1_100)]), Some("air"), true, 1_000);
        assert_eq!(rows.agents[0].status_age_ms, 0);
        assert_eq!(rows.agents[0].last_seen_age_ms, Some(0));
    }

    #[test]
    fn no_peers_still_yields_both_arrays_and_the_local_rows() {
        // Sync off. `peers: []` here means "this dashboard cannot receive rows",
        // which is why `sync_listening` ships alongside it — the caller must not
        // read the empty list as "the other machine has nothing". The flag itself
        // is not asserted here: it is a pass-through parameter, sourced from the
        // running listener (`sync::SyncListening`) rather than derived, so there
        // is no predicate at this level that could be wrong.
        let rows = agent_roster(&[session("transcripts", Status::Idle, None, 0)], Some(&[]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(rows.peers.is_empty());
        assert!(!rows.sync_listening);
        assert_eq!(rows.agents.len(), 1);
    }

    #[test]
    fn a_peer_row_counts_the_sessions_attributed_to_it() {
        // A live peer with zero sessions still appears — that is the only way to
        // tell "up and idle" from "silent", and no row can express it.
        let sessions = [
            session("transcripts", Status::Working, None, 0),
            session("chrome/transcripts", Status::Idle, Some("chrome"), 0),
            session("chrome/whats next", Status::Done, Some("chrome"), 0),
        ];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("chrome", 900), ("mini", 500)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers.len(), 2);
        assert_eq!(rows.peers[0].device, "chrome");
        assert_eq!(rows.peers[0].sessions, 2, "local rows are not attributed to a peer");
        assert_eq!(rows.peers[1].sessions, 0, "a live peer with no sessions is still a peer");
        assert_eq!(rows.peers[1].last_seen_age_ms, 500);
    }

    #[test]
    fn a_peer_sharing_this_devices_name_is_not_credited_with_local_rows() {
        // `device_name` bootstraps from the hostname, so two boxes can genuinely
        // carry the same name — and the repo's own localhost-observer sync setup
        // makes this device its own peer on purpose. Matching a peer's rows by
        // name alone folds every local session into its count, inflating a number
        // the caller reads as "how much is over there".
        let sessions = [
            session("transcripts", Status::Working, None, 0),
            session("air/whats next", Status::Idle, Some("air"), 0),
        ];
        let rows = agent_roster(&sessions, Some(&[]), &|_| None, &devices(&[("air", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers.len(), 1);
        assert_eq!(rows.peers[0].sessions, 1, "only the remote row counts, not the local one sharing the name");
    }

    #[test]
    fn an_unnamed_local_device_reports_null_rather_than_a_sentinel() {
        // `sync.device_name` is bootstrapped from the hostname at startup, but a
        // bypassed bootstrap must not invent a name: a stand-in like "local" could
        // collide with a real peer's name and mis-attribute rows.
        let rows = agent_roster(&[session("transcripts", Status::Idle, None, 0)], Some(&[]), &|_| None, &devices(&[]), None, false, 1_000);
        assert_eq!(rows.device, None);
        assert_eq!(rows.agents[0].device, None);
        assert!(rows.agents[0].local, "unnamed is still unambiguously local");
    }

    fn live(chat_id: &str, activity: Activity, sessions: usize) -> LiveSession {
        LiveSession { chat_id: chat_id.to_string(), name: Some(chat_id.to_string()), activity, activity_age_ms: Some(600), sessions, session_ids: Vec::new() }
    }

    #[test]
    fn an_unreadable_registry_is_null_not_an_empty_list() {
        // The distinction the whole `Option` exists for. `[]` asserts this
        // machine is running nothing; `null` says we could not look. They are
        // reachable by ordinary means — no `sessions/` directory, or a
        // node-based install whose records never survive the image check — and
        // collapsing them would let the roster claim an absence it never
        // established, which is precisely what a delivery caller would act on.
        let unreadable = agent_roster(&[], None, &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(unreadable.registry_only.is_none(), "an unreadable registry must not read as an empty machine");

        let empty = agent_roster(&[], Some(&[]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(empty.registry_only.as_deref(), Some(&[][..]), "a readable but empty registry is a real answer");

        // And the two must serialize differently, or the distinction dies at the wire.
        let unreadable_json = serde_json::to_string(&unreadable).unwrap();
        let empty_json = serde_json::to_string(&empty).unwrap();
        assert!(unreadable_json.contains("\"registry_only\":null"), "got {unreadable_json}");
        assert!(empty_json.contains("\"registry_only\":[]"), "got {empty_json}");
    }

    #[test]
    fn a_registry_session_the_hooks_never_saw_appears_as_registry_only() {
        // The whole point of the stage: a session idle since before the dashboard
        // started fires no hook, so it was invisible for as long as it stayed idle.
        // Claude Code's registry knows it the whole time.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Idle, 1)]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(rows.agents.is_empty());
        assert_eq!(rows.registry_only.as_ref().unwrap().len(), 1);
        let row = &rows.registry_only.as_ref().unwrap()[0];
        assert_eq!(row.id, "printlab");
        assert_eq!(row.project, "printlab", "a local id is never namespaced, so the two keys agree");
        assert_eq!(row.device.as_deref(), Some("air"), "the registry describes this machine");
        assert_eq!(row.name.as_deref(), Some("printlab"));
        assert_eq!(row.activity, Activity::Idle);
        assert_eq!(row.activity_age_ms, Some(600));
        assert_eq!(row.sessions, 1);
    }

    #[test]
    fn a_hook_row_wins_the_same_cwd_and_the_registry_row_is_dropped() {
        // One row, hook-derived, with its real status and label intact. The
        // registry's id derivation is the same cwd derivation, which is what makes
        // the dedupe plain equality.
        let sessions = [session("transcripts", Status::Working, None, 900)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents.len(), 1);
        assert_eq!(rows.agents[0].status, Status::Working);
        assert_eq!(rows.agents[0].label, "label");
        assert!(rows.registry_only.as_ref().unwrap().is_empty(), "the project is already in `agents`");
    }

    #[test]
    fn a_registry_busy_never_overwrites_a_hook_derived_blocked() {
        // The reason the registry is a second array and not a status source: a
        // session parked on a question is `blocked` here and reads `busy` (or
        // `idle`) there, and `blocked` is the state a caller most needs not to
        // misread. Nothing about the hook row moves.
        let sessions = [session("transcripts", Status::Blocked, None, 500)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Busy, 1)]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents[0].status, Status::Blocked);
        assert_eq!(rows.agents[0].status_age_ms, 500);
        assert!(rows.registry_only.as_ref().unwrap().is_empty());
    }

    #[test]
    fn a_registry_row_carries_no_status_label_or_last_seen_age() {
        // Serialized rather than field-checked: the guarantee is about the wire,
        // where a caller reading `status`/`label` off a row that has neither would
        // be reading a state the registry cannot express.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Busy, 1)]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        let body = serde_json::to_string(&rows).unwrap();
        assert!(body.contains(r#""activity":"busy""#), "the registry's own word, under its own key");
        assert!(!body.contains(r#""status""#), "no dashboard status anywhere in this body");
        assert!(!body.contains(r#""label""#));
        assert!(!body.contains("last_seen_age_ms"), "there is no push channel behind a local row");
        assert!(!body.contains(r#""local""#), "the array is local by construction; a constant is not information");
    }

    #[test]
    fn a_registry_row_and_a_remote_row_can_share_a_project_without_either_being_dropped() {
        // `project` is the cross-machine key, so the same repo checked out on both
        // machines is *supposed* to collide. Deduping across machines would delete
        // the remote row's evidence that the project also runs over there.
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 0)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &|_| None, &devices(&[("chrome", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.agents.len(), 1, "the remote row survives");
        assert_eq!(rows.agents[0].project, "transcripts");
        assert_eq!(rows.registry_only.as_ref().unwrap().len(), 1, "so does the local registry row for the same project");
        assert_eq!(rows.registry_only.as_ref().unwrap()[0].project, "transcripts");
    }

    #[test]
    fn registry_rows_do_not_inflate_a_peers_session_count() {
        // `peers[].sessions` counts `!a.local` rows in `agents`; registry rows are
        // in neither, including when a peer shares this device's name — the case
        // that already broke the count once.
        let sessions = [session("air/whats next", Status::Idle, Some("air"), 0)];
        let registry = [live("transcripts", Activity::Idle, 1), live("printlab", Activity::Busy, 1)];
        let rows = agent_roster(&sessions, Some(&registry), &|_| None, &devices(&[("air", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers[0].sessions, 1, "only the remote row counts");
        assert_eq!(rows.registry_only.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn a_collapsed_cwd_reports_how_many_sessions_it_stands_for() {
        // Two interactive sessions in one directory (a fork migration) are one
        // dashboard row, since a row's identity is the cwd. The count is reported
        // so the collapse is stated rather than emergent.
        let rows = agent_roster(&[], Some(&[live("landlord", Activity::Busy, 2)]), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.registry_only.as_ref().unwrap().len(), 1);
        assert_eq!(rows.registry_only.as_ref().unwrap()[0].sessions, 2);
    }

    #[test]
    fn an_unnamed_local_device_leaves_a_registry_rows_device_null() {
        // Same rule as an `agents` row: no invented stand-in name, which could
        // collide with a real peer's.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Idle, 1)]), &|_| None, &devices(&[]), None, false, 1_000);
        assert_eq!(rows.registry_only.as_ref().unwrap()[0].device, None);
    }

    #[test]
    fn origin_blocked_lets_the_hook_and_curl_through() {
        // Neither sends an Origin header at all.
        assert!(!origin_blocked(&HeaderMap::new()));
    }

    #[test]
    fn origin_blocked_stops_a_browser_page_reading_the_roster() {
        // The roster carries project names and labels, so a page the user has open
        // is an exfiltration path even though the route mutates nothing.
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://evil.example".parse().unwrap());
        assert!(origin_blocked(&headers));
    }

    #[test]
    fn origin_blocked_allows_the_null_origin() {
        // file:// and data: documents send "null"; the guard has always let them by.
        let mut headers = HeaderMap::new();
        headers.insert("origin", "null".parse().unwrap());
        assert!(!origin_blocked(&headers));
    }


    #[test]
    fn clear_permitted_when_the_owning_session_ends_it() {
        // The ordinary case, `/clear` included: one instance holds the cwd, so
        // the end signal carries the same session_id that claimed the row.
        // (`/clear` fires SessionEnd under the old id and mints the new one on
        // the following SessionStart, so it matches here.)
        assert!(clear_permitted(Some("cc152457"), "cc152457"));
    }

    #[test]
    fn clear_refused_from_a_sibling_sharing_the_cwd() {
        // A terminal session forked into a background one: both derive the same
        // cwd chat_id, so exiting the abandoned tab must not drop the live row.
        assert!(!clear_permitted(Some("add18820"), "cc152457"));
    }

    #[test]
    fn clear_refused_even_when_the_ending_session_is_already_dying() {
        // The regression this guard exists for. Keying on `agent_pid` let a real
        // SessionEnd through: the hook resolves that pid by walking its ancestors
        // for a live `claude` image, and a session being killed has none, so it
        // reported null and "unknown ownership" waved the removal past. The
        // session_id is in the payload either way.
        assert!(!clear_permitted(Some("add18820"), "83820-is-dying"));
    }

    #[test]
    fn clear_permitted_when_ownership_is_unknown() {
        // Nothing claimed since a restart, or a payload with no session_id.
        // Unknown ownership defers to the authoritative end signal rather than
        // stranding the row.
        assert!(clear_permitted(None, "cc152457"));
        assert!(clear_permitted(Some("cc152457"), ""));
        assert!(clear_permitted(None, ""));
    }

    #[test]
    fn drift_present_clears_regardless_of_turn_shape() {
        // Adherence on any turn (completion or handback) clears drift.
        assert_eq!(drift_action(true, true, false).0, DriftAction::Clear);
        assert_eq!(drift_action(true, true, true).0, DriftAction::Clear);
    }

    #[test]
    fn drift_unconfirmed_absence_is_held_never_flagged() {
        // `!seen`: the instruction may never have reached the model — hold, don't flag.
        assert_eq!(drift_action(false, false, false).0, DriftAction::Hold);
        assert_eq!(drift_action(false, false, true).0, DriftAction::Hold);
    }

    #[test]
    fn drift_completion_turn_drop_confirms() {
        // The only path that surfaces drift: a settled completion turn dropped the
        // marker after prior adherence.
        assert_eq!(drift_action(false, true, false).0, DriftAction::Confirm);
    }

    #[test]
    fn drift_handback_turn_drop_is_deferred_not_confirmed() {
        // The regression this guards: a `/commit` reflection ending on a question is a
        // `Blocked` handback; the model legitimately drops the hidden marker there and
        // resumes on the next completion turn, so a drop here must NOT ping.
        assert_eq!(drift_action(false, true, true).0, DriftAction::Hold);
    }

    #[test]
    fn startup_mints_and_stores_a_fresh_unseen_nonce() {
        let ns = NonceStore::new();
        let n = session_start_nonce(&ns, "proj", "startup", 1000).expect("startup mints");
        assert_eq!(ns.get("proj"), Some((n, false)), "the minted nonce is stored, unseen");
    }

    #[test]
    fn clear_rotates_even_when_a_nonce_exists() {
        // `/clear` wipes the model's context, so the marker instruction is gone —
        // rotating to a fresh nonce is correct (restored-history stale markers are
        // scrubbed by `strip_response_marker`).
        let ns = NonceStore::new();
        let first = session_start_nonce(&ns, "proj", "startup", 1000).unwrap();
        let after_clear = session_start_nonce(&ns, "proj", "clear", 2000).unwrap();
        assert_ne!(first, after_clear, "clear must rotate the nonce");
        assert_eq!(ns.get("proj").map(|(n, _)| n), Some(after_clear));
    }

    #[test]
    fn resume_reuses_the_existing_nonce_and_preserves_seen() {
        // The regression this guards: a `resume` re-fires SessionStart, but the
        // model keeps its prior context (and original marker), so the nonce must
        // NOT rotate — else the backend expects a marker the model never emits and
        // the row is stuck Pending forever.
        let ns = NonceStore::new();
        let first = session_start_nonce(&ns, "proj", "startup", 1000).unwrap();
        ns.mark_seen("proj"); // confirmed adherent (green)
        let resumed = session_start_nonce(&ns, "proj", "resume", 2000);
        assert_eq!(resumed.as_ref(), Some(&first), "resume keeps context → same marker");
        assert_eq!(ns.get("proj"), Some((first, true)), "reuse keeps the session green");
    }

    #[test]
    fn compact_reuses_like_resume() {
        let ns = NonceStore::new();
        let first = session_start_nonce(&ns, "proj", "startup", 1000).unwrap();
        assert_eq!(session_start_nonce(&ns, "proj", "compact", 2000), Some(first));
    }

    #[test]
    fn resume_with_no_retained_nonce_is_untracked_not_minted() {
        // App restarted mid-session: nothing to reuse. Returning None skips the
        // injection rather than minting a nonce the model isn't emitting (which
        // would recreate the conflict).
        let ns = NonceStore::new();
        assert_eq!(session_start_nonce(&ns, "proj", "resume", 1000), None);
        assert_eq!(ns.get("proj"), None, "a resume miss must not mint");
    }

    #[test]
    fn unknown_source_mints_like_a_fresh_start() {
        // A missing/unknown `source` is treated as a fresh start — mint — never a
        // silent reuse.
        let ns = NonceStore::new();
        assert!(session_start_nonce(&ns, "proj", "", 1000).is_some());
    }

    // -------- the message route's two gates --------

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    /// The CSRF check the message handler repeats: there is no tower layer on
    /// this router, so a route that forgets it is simply unprotected.
    #[test]
    fn a_browser_origin_is_blocked_on_the_message_route() {
        assert!(origin_blocked(&headers(&[("origin", "https://evil.example")])));
        assert!(!origin_blocked(&headers(&[("origin", "null")])), "file:// and data: pages");
        assert!(!origin_blocked(&HeaderMap::new()), "curl and the hook send no Origin");
    }

    /// The extra gate this route alone carries. A page rebound to loopback sends
    /// no `Origin` — same-origin — so the CSRF check waves it through; it still
    /// carries the attacker's own hostname in `Host`, which is what this
    /// catches. The accepted rebinding gap stays accepted for the hook routes,
    /// where the stake is a status write, not a turn started on another machine.
    #[test]
    fn a_rebound_hostname_is_refused_while_loopback_names_pass() {
        for host in ["127.0.0.1:9077", "localhost:9077", "localhost", "[::1]:9077", "127.0.0.1", "127.0.0.2:9077"] {
            assert!(host_is_loopback(&headers(&[("host", host)])), "{host}");
        }
        for host in ["evil.example:9077", "dashboard.internal", "10.0.0.5:9077", "[fd7a:115c:a1e0::1]:9077"] {
            assert!(!host_is_loopback(&headers(&[("host", host)])), "{host}");
        }
        assert!(!host_is_loopback(&HeaderMap::new()), "HTTP/1.1 requires Host; its absence is not a client we serve");
    }

    /// The refusal has to name the tool that does the job properly, or a caller
    /// learns only that it failed. `SendMessage` carries a kernel-verified
    /// sender and a reply address; this route has neither.
    #[test]
    fn the_local_refusal_names_send_message() {
        let detail = "this session is on this machine; use Claude Code's `SendMessage`, which carries a kernel-verified sender identity and a reply address this route destroys";
        let receipt = Receipt::new(Outcome::Refused, "", "transcripts", None).because("local_target").detailed(detail);
        assert!(receipt.detail.as_deref().is_some_and(|d| d.contains("SendMessage")));
        assert_eq!(receipt.reason.as_deref(), Some("local_target"));
        assert!(!receipt.observed.to_ascii_lowercase().contains("deliver"));
    }
}
