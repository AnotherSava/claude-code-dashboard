//! Multi-device session sync. Every device runs the dashboard; each pushes
//! its *local* sessions to the peers in `config.sync.peers` and renders the
//! remote sessions it receives (stored in `AppState::remote`, merged into the
//! frontend payload by `commands::resolved_snapshot`).
//!
//! Single-writer model: each device is authoritative for its own sessions.
//!
//! **Push notifies, pull moves content.** A push carries a full snapshot of the
//! sender's session metadata (receiver wholesale-replaces it per device;
//! removal = absence) and, per session, nothing but a `dialog_tip` — the newest
//! dialog timestamp the sender holds. The receiver compares each tip against
//! what it actually holds and fetches the difference itself over
//! `GET /api/sync/{dialog,usage}`. Usage samples work the same way via
//! `usage_tip`.
//!
//! The split is deliberate. Change events originate at the sender — only it
//! knows *when* a session moved — so notification must be pushed or peers would
//! have to poll several times a second to keep a live widget fresh. But dialogs
//! run to hundreds of KB, so they can't ride every push, and the moment content
//! is sent incrementally the sender needs to know how far the receiver got.
//! That is a guess about someone else's contents, and it went stale silently:
//! a delta the receiver discarded still counted as delivered, and the session
//! stayed stranded for the rest of the sender's run. Only the receiver knows
//! what it holds, so only the receiver decides what to ask for. The sender is
//! stateless — the same snapshot goes to every peer every cycle, a failed push
//! costs a retry and nothing else, and there is no watermark to persist,
//! reconcile, or leak.
//!
//! Everything the receiver stores is therefore derived from data it can see:
//! its own newest held timestamp, never a number tracked alongside it.
//!
//! **Two gates, not one.** Every request passes [`guard`] before any route
//! runs: its source address must be inside the scope (`config.sync.bind_scope`,
//! by default the tailnet plus loopback) *and* it must carry the shared bearer
//! token. With no token configured sync is fully disabled — never run
//! unauthenticated.
//!
//! The listener is bound as narrowly as it can be — this device's Tailscale
//! addresses, not `0.0.0.0` — for the same reason the user's sshd is scoped to
//! the tailnet: a home router should not be the last line of defence, and a
//! socket that was never on the LAN interface cannot be scanned, handshaken or
//! fed a malformed request from it. But a narrow bind fails outright when
//! Tailscale is not up yet (the dashboard and the VPN both start at login, so
//! that race is ordinary, not exotic), and a listener that never started is
//! silent, permanent and worse than a wide one. So the bind degrades to all
//! interfaces and says so at `warn!` — while [`guard`] keeps rejecting every
//! non-tailnet source, so the degrade widens the *socket*, never the trust.
//!
//! That is the whole reason both gates exist. The bind decides what can reach
//! us and cannot self-heal (it is chosen once at startup — see
//! [`run_listener`]); the source check decides what we will answer, re-evaluates
//! per request, and covers the degraded bind, a stale socket left behind by a
//! tailnet address change, and any route added to this router later. Neither
//! replaces the bearer token: they bound *who may attempt* auth, and buy nothing
//! against an attacker who already holds the token or sits on the tailnet.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::commands::{emit_sessions_updated_remote, emit_usage_limits_updated, now_ms};
use crate::config::{ConfigState, SyncBindScope};
use crate::peer_message::{build_content, deliver_to_inbox, from_id, MessageDedupe, Outcome, Receipt, Relayed};
use crate::remote_history::RemoteHistoryStore;
use crate::session_registry::InboxLookup;
use crate::remote_usage::RemoteUsageStore;
use crate::state::{merge_dialog_entries, AgentSession, AppState, DialogEntry, RemoteDevice};
use crate::usage_history::{UsageHistoryRecord, UsageHistoryStore};

/// Coalesce window after a state change before pushing.
const DEBOUNCE_MS: u64 = 300;
/// Periodic push even without local changes — keeps peers' `last_seen` fresh.
const HEARTBEAT_SECS: u64 = 30;
/// Drop a remote device after this long without a push (3 missed heartbeats).
const REMOTE_TTL_MS: i64 = 90_000;
/// Poked by `commands::emit_sessions_updated` on every state transition; the
/// pusher debounces and ships local sessions to all peers.
pub struct SyncDirty(pub Arc<Notify>);

/// Wire shape for `POST /api/sync`.
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncPush {
    pub device_name: String,
    /// The sender's own sync listener port; combined with the socket peer IP
    /// it gives the receiver the address to pull dialog and usage ranges from.
    pub listen_port: u16,
    pub sessions: Vec<SessionSync>,
    /// Newest local usage-sample timestamp, or `0` when there are none. Same
    /// role as `SessionSync::dialog_tip`: the receiver pulls the records above
    /// whatever it already holds for this device.
    #[serde(default)]
    pub usage_tip: i64,
    /// Highest local token-record `seq`, or `0` when there are none. Deliberately
    /// a sequence rather than a timestamp: token records are appended in scan
    /// order, so a `ts` tip would go backwards and silently strand everything
    /// below it — see `remote_tokens`. `serde(default)` keeps a peer running an
    /// older build parseable; it simply advertises 0 and contributes nothing.
    #[serde(default)]
    pub token_tip: u64,
    /// Live local sessions from Claude Code's own registry — the *second*
    /// session source, mirrored across the wire so a peer's roster can see
    /// them.
    ///
    /// Without this, discovery is strictly narrower than delivery, which is the
    /// wrong way round and cost a real message: on 2026-08-30 a live session was
    /// absent from `/api/agents` while the relay could reach it perfectly well,
    /// because `agent_roster` unions the registry for *this* machine only. An
    /// agent following the documented "check the roster first" concludes the
    /// target is gone.
    ///
    /// `None` means "this device has no registry answer", which covers both an
    /// unreadable registry and a peer old enough not to send the field. Those
    /// are the same fact for the receiver — no answer — so collapsing them
    /// over-claims nothing.
    #[serde(default)]
    pub registry_sessions: Option<Vec<RegistrySync>>,
}

/// One live session from a peer's Claude Code registry.
///
/// Carries no `status` or `label`, because the registry has neither: it knows
/// `idle`/`busy` only, which cannot express `blocked`, `waiting` or `error`.
/// That is why these stay a separate array all the way through rather than
/// being folded into `sessions` — see `http_server::RegistryRow`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RegistrySync {
    /// The sender's cwd-derived id, *not* namespaced — the receiver stamps its
    /// own device prefix, exactly as it does for `SessionSync`.
    pub chat_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub activity: crate::session_registry::Activity,
    /// Age of the activity reading **as measured by the sender at push time**.
    ///
    /// A duration rather than the absolute stamp `AgentSession` ships, and this
    /// one is the better shape: the receiver reports `age_at_push + (now -
    /// last_seen)`, which is two durations added, so it needs no clock agreement
    /// and carries **no skew at all**. `AgentRow::status_age_ms` has to document
    /// a skew it cannot remove; this does not.
    #[serde(default)]
    pub activity_age_ms: Option<i64>,
    pub sessions: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionSync {
    /// Full metadata snapshot with `dialog` stripped to the tip below, raw
    /// local id, `origin`/`display_name` unset — the receiver namespaces and
    /// stamps.
    pub session: AgentSession,
    /// Newest dialog timestamp the origin holds for this session, or `0` for an
    /// empty dialog. The receiver compares it against its own newest held entry
    /// and pulls the difference itself — no dialog content rides the push.
    ///
    /// This is the whole reason the sender is stateless. A push carrying deltas
    /// had to remember, per peer and per session, how far it believed that peer
    /// had got; being a guess about *someone else's* contents it went stale
    /// silently (a discarded delta still acknowledged, a merge dropping an
    /// entry) and stranded the session for the rest of the run. A tip is a fact
    /// about the sender's own data, and the party that acts on it is the only
    /// one that can check it.
    #[serde(default)]
    pub dialog_tip: i64,
}

/// Wire shape for `POST /api/sync/message` — one cross-machine message on the
/// dashboard-to-dashboard hop.
///
/// It lives here with `SyncPush` because both ends of the hop are in this file
/// and one definition compiled into both binaries is what makes the shape
/// unforgeable across a version skew. Its *reply*, `peer_message::Receipt`,
/// deliberately lives elsewhere: the receipt's wording is bound to what a raw
/// socket writer can observe, which is that module's whole subject.
///
/// Every field carries `#[serde(default)]` for the same reason `token_tip` does
/// — a peer on an older build must parse a newer envelope rather than failing
/// the request, and a peer on a newer build must not be broken by a field this
/// one does not send.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct MessageEnvelope {
    /// The sending *machine*, as it names itself. Used to namespace the dedupe
    /// key and to build the claim header; never trusted as authentication —
    /// nothing here is, which is exactly what the header says out loud.
    #[serde(default)]
    pub origin_device: String,
    /// Minted by the sending dashboard, so a retried hop is recognizable as the
    /// same message on the machine that owns the socket.
    #[serde(default)]
    pub message_id: String,
    /// The de-namespaced project id, resolved against the *receiver's* own
    /// registry. The sender never claims the session exists over here.
    #[serde(default)]
    pub target_project: String,
    /// The originating agent's own id — a claim, presented to the receiving
    /// model as one, and the key that gives each sender its own admission
    /// bucket on the receiver (see `peer_message::from_id`).
    #[serde(default)]
    pub from_agent: String,
    #[serde(default)]
    pub from_label: Option<String>,
    #[serde(default)]
    pub text: String,
    /// The exact `{device}/{project}` address a reply goes to, minted by the
    /// sending dashboard in `http_server::post_message` — the only place holding
    /// both halves *exactly*.
    ///
    /// **Carried rather than derived**, and that is the whole point of the
    /// field. The obvious alternative is for the receiver to reconstruct it from
    /// the frame's `from` (`did:ccdash-{device}-{agent}`), which is impossible:
    /// `peer_message::from_id` lowercases and collapses both halves to
    /// `[a-z0-9-]`, while `peer_message::resolve_message_target` compares
    /// exactly, deliberately. `Some-Laptop.local` arrives as
    /// `some-laptop-local` and project `tauri dashboard` as
    /// `tauri-dashboard`; neither round-trips. A receiver that derived an
    /// address would produce a confident wrong one.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// The `message_id` this message answers, when it is a reply. Rendered into
    /// the envelope and otherwise **inert** — nothing here branches on it. It
    /// exists so two overlapping exchanges with one session are distinguishable
    /// by the agents, not so the dashboard can match them up.
    #[serde(default)]
    pub in_reply_to: Option<String>,
}

/// One dialog range the receiver decided it is missing: `since` is its newest
/// held timestamp, so the origin returns strictly newer entries.
#[derive(Debug, PartialEq)]
struct DialogPull {
    raw_id: String,
    since: i64,
}

/// True when the request carries `Authorization: Bearer <token>` matching the
/// configured shared secret. No configured token = reject everything.
///
/// The credential comparison goes through [`tokens_match`], never `==`: `str`
/// equality tests the lengths and then hands off to `memcmp`, which returns at
/// the first differing word, so the rejection latency encodes how much of the
/// secret the caller already guessed.
fn bearer_ok(headers: &HeaderMap, token: Option<&str>) -> bool {
    let Some(expected) = token else {
        return false;
    };
    // Checked before the comparison rather than folded into it. An empty
    // configured token is a *config* state, not attacker input, so returning
    // early on it leaks nothing about any secret — there is none. `lib.rs`
    // already refuses to start the listener on one; this is the second gate,
    // for a token emptied by hot-reload under a listener that is already up.
    if expected.is_empty() {
        return false;
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| tokens_match(t.as_bytes(), expected.as_bytes()))
}

/// Byte-string equality with no data-dependent early exit: every difference —
/// byte *and* length — folds into one accumulator, so every rejection walks one
/// path at one cost, whether the first byte was wrong or only the last.
///
/// The trip count is the *presented* token's length, never the secret's; bytes
/// past the end of `expected` fold in `p ^ 0` instead of ending the loop, so the
/// loop cannot be timed to size the secret. That is exactly what the vetted
/// crates decline to do — `subtle`'s `ConstantTimeEq for [T]` and `ring`'s
/// `verify_slices_are_equal` both short-circuit on unequal lengths, treating
/// length as public — so neither was added as a dependency; each would leave the
/// leak this replaces while adding a version to keep in step with `rustls`.
///
/// `black_box` is best-effort insurance, not a guarantee: nothing in the
/// language stops LLVM recognising an OR-accumulate over two byte strings as the
/// `bcmp` idiom and restoring the early exit. Only checked-in assembly would
/// prove otherwise, which is not worth it at this stake.
///
/// Honest scope: across a tailnet the noise floor (WireGuard, scheduling, Wi-Fi)
/// is microseconds against a per-word difference of about a nanosecond, so the
/// `==` this replaces was not realistically exploitable from off-box. This
/// removes the class; it does not close a live hole.
fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    let mut diff = (presented.len() ^ expected.len()) as u64;
    for (i, &p) in presented.iter().enumerate() {
        diff |= u64::from(p ^ expected.get(i).copied().unwrap_or(0));
    }
    std::hint::black_box(diff) == 0
}

/// Unwrap `::ffff:a.b.c.d` to the v4 address it carries. Every classification
/// below runs on the result: a dual-stack socket reports v4 peers in the mapped
/// form, and a v4-shaped check applied to the mapped form matches nothing —
/// which would reject every tailnet peer and every loopback caller with no
/// branch and no log.
/// Base URL for pulling content back from the peer that just pushed to us.
///
/// An IPv6 literal **must** be bracketed in a URL, or the trailing `:<port>`
/// reads as another hextet and the whole thing fails to parse. This was latent
/// and unreachable while the listener bound `0.0.0.0` (v4-only, so `addr.ip()`
/// could never be v6); binding the tailnet's `fd7a:115c:a1e0::/48` address made
/// it reachable, and the failure is silent in the worst way — the push itself
/// answers 204 so metadata and statuses look healthy, while every dialog, usage
/// and token pull built from this string dies in `reqwest` with the remote
/// content simply never arriving. Mapped v4 is unwrapped first so a
/// `::ffff:100.x.x.x` arrival yields the plain dotted form rather than a
/// bracketed mapped literal.
fn origin_url(ip: IpAddr, port: u16) -> String {
    match unmap(ip) {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
    }
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// True for an address Tailscale hands out: the CGNAT block 100.64.0.0/10 for
/// v4, the ULA prefix fd7a:115c:a1e0::/48 for v6. Both are checked because a
/// tailnet node has both and a peer dialled by MagicDNS name can arrive over
/// either — a v4-only test would reject a v6 peer silently.
///
/// The v4 test is the /10 mask, not a `"100."` string prefix and not a /8:
/// 100.0.0.1 and 100.128.0.1 are ordinary public addresses.
///
/// This is a filter, not proof of tailnet membership. 100.64.0.0/10 is the real
/// CGNAT range and carriers assign it directly (Starlink, T-Mobile Home, most
/// mobile hotspots), so on such a network an unrelated host can present an
/// allowed source; `local_tailnet_addrs` cross-checks the default route for the
/// same reason. A Tailscale subnet router also puts every host behind it in
/// range. The bearer token, not this check, is what authenticates a peer — and
/// who is on the tailnet at all is a Tailscale ACL question.
fn is_tailnet(ip: IpAddr) -> bool {
    match unmap(ip) {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Whether a request from `ip` may be answered at all — evaluated in [`guard`]
/// *before* the token is read, so an out-of-scope caller never reaches the
/// comparison and gets no oracle from it, timing or otherwise.
///
/// Loopback stays accepted under every scope: the documented sync test harness
/// runs an observer peer on 127.0.0.1 (see the `debug_sync_fake_peer` project
/// memory), and the alternative — special-casing it in the test setup — would
/// mean the tested path is not the shipped one.
///
/// The source address is the kernel's TCP peer address, not a header. There is
/// no proxy in front of this listener, so there is no `X-Forwarded-For` shape
/// to spoof; an attacker would have to forge a whole TCP handshake off-path.
fn source_allowed(scope: SyncBindScope, ip: IpAddr) -> bool {
    match scope {
        SyncBindScope::Any => true,
        SyncBindScope::Tailnet => {
            let ip = unmap(ip);
            ip.is_loopback() || is_tailnet(ip)
        }
    }
}

/// Build the receiver-side state for one device from an incoming push:
/// namespace ids to "{device}/{raw_id}", stamp `origin`, and carry over the
/// dialog accumulated so far (sessions absent from the snapshot drop out by not
/// being carried). Sessions without an in-memory predecessor seed their dialog
/// from `persisted` — the on-disk copy a dashboard restart would otherwise have
/// discarded.
///
/// No dialog content arrives here. Instead each session whose advertised
/// `dialog_tip` is newer than our own newest held entry yields a [`DialogPull`]
/// for the caller to fetch. Because the range is derived from what we hold at
/// the moment we ask, a failed or dropped fetch is self-correcting: nothing
/// recorded that we had it, the next push re-advertises the same tip, and we
/// ask again.
fn ingest(
    device: &str,
    sessions: Vec<SessionSync>,
    registry_sessions: Option<Vec<RegistrySync>>,
    identity: crate::tailnet::Attestation,
    prev: Option<&RemoteDevice>,
    persisted: &HashMap<String, Vec<DialogEntry>>,
    now: i64,
    origin_addr: String,
) -> (RemoteDevice, Vec<DialogPull>) {
    let mut out = Vec::with_capacity(sessions.len());
    let mut pulls = Vec::new();
    for item in sessions {
        let mut s = item.session;
        let raw_id = s.id.clone();
        s.id = format!("{device}/{raw_id}");
        s.origin = Some(device.to_string());
        s.display_name = None; // receiver's custom names win at emit time
        // Carry the dialog we already hold across the metadata replace: in
        // memory first, else the on-disk copy a restart would otherwise drop.
        let dialog = prev
            .and_then(|p| p.sessions.iter().find(|ps| ps.id == s.id))
            .map(|ps| ps.dialog.clone())
            .or_else(|| persisted.get(&s.id).cloned())
            .unwrap_or_default();
        let held = dialog.iter().map(|e| e.timestamp).max().unwrap_or(0);
        if item.dialog_tip > held {
            pulls.push(DialogPull { raw_id, since: held });
        }
        s.dialog = dialog;
        out.push(s);
    }
    (RemoteDevice { sessions: out, last_seen: now, origin_addr, registry_sessions, identity }, pulls)
}

/// Auth and source scope are already settled by [`guard`]; this handler only
/// has to trust the address, which it uses to build the pull-back URL.
async fn post_sync(
    State(app): State<AppHandle>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(mut push): Json<SyncPush>,
) -> Result<StatusCode, StatusCode> {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let cfg = cfg_state.snapshot();
    let Some(state) = app.try_state::<AppState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if push.device_name.is_empty() || push.device_name == cfg.sync.device_name {
        tracing::warn!(device = %push.device_name, "sync push rejected: empty or same device_name as ours");
        return Err(StatusCode::BAD_REQUEST);
    }
    // The same check the message route makes, and it belongs here for a reason
    // that has nothing to do with messaging: `device_name` decides which device's
    // rows these become, and it is a field the sender picks. Any token-holder
    // could therefore push sessions attributed to the user's own laptop. Only a
    // binding that is *contradicted* refuses; an unbound device stays `Claimed`,
    // which is exactly today's behaviour.
    let identity = match app.try_state::<crate::tailnet::TailnetResolver>() {
        Some(r) => r.attest_peer(addr.ip(), &push.device_name, &cfg.sync.peer_identity).0,
        None => crate::tailnet::Attestation::Claimed,
    };
    if identity == crate::tailnet::Attestation::Mismatch {
        tracing::warn!(device = %push.device_name, peer_ip = %addr.ip(), "sync push rejected: device is bound to a different Tailscale node");
        return Err(StatusCode::FORBIDDEN);
    }
    tracing::debug!(
        device = %push.device_name,
        sessions = push.sessions.len(),
        "sync push received"
    );
    let origin_addr = origin_url(addr.ip(), push.listen_port);
    let now = now_ms();
    let store = app.try_state::<RemoteHistoryStore>();
    let persisted = store.as_ref().map(|s| s.device_dialogs(&push.device_name)).unwrap_or_default();
    let usage_tip = push.usage_tip;
    let token_tip = push.token_tip;
    let device_name = push.device_name.clone();
    let registry_sessions = push.registry_sessions.take();
    let pulls = {
        let mut remote = state.remote.lock().unwrap();
        let prev = remote.get(&push.device_name);
        let (device, pulls) = ingest(&push.device_name, push.sessions, registry_sessions, identity, prev, &persisted, now, origin_addr.clone());
        remote.insert(push.device_name.clone(), device);
        pulls
    };
    emit_sessions_updated_remote(&app);
    // Content is fetched, not received: we know exactly what we hold, so we ask
    // for the remainder ourselves rather than have the sender guess. Spawned
    // after the lock is released; each merge re-emits when it lands.
    for pull in pulls {
        fetch_dialog_range(app.clone(), device_name.clone(), origin_addr.clone(), pull);
    }
    let usage_held = app
        .try_state::<RemoteUsageStore>()
        .map(|s| s.newest_ts(&device_name))
        .unwrap_or(0);
    if usage_tip > usage_held {
        fetch_usage_range(app.clone(), device_name.clone(), origin_addr.clone(), usage_held);
    }
    let token_held = app
        .try_state::<crate::remote_tokens::RemoteTokenStore>()
        .map(|s| s.newest_seq(&device_name))
        .unwrap_or(0);
    if token_tip > token_held {
        fetch_token_range(app.clone(), device_name, origin_addr, token_held);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DialogQuery {
    id: String,
    #[serde(default)]
    since: i64,
}

/// Catch-up endpoint: a peer that lost its accumulated copy (restart) asks
/// for our *local* session's dialog entries newer than `since`.
async fn get_dialog(State(app): State<AppHandle>, Query(q): Query<DialogQuery>) -> Result<Json<Vec<DialogEntry>>, StatusCode> {
    let Some(state) = app.try_state::<AppState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let sessions = state.sessions.lock().unwrap();
    let Some(s) = sessions.iter().find(|s| s.id == q.id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Json(s.dialog.iter().filter(|e| e.timestamp > q.since).cloned().collect()))
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    since: i64,
}

/// The usage counterpart of [`get_dialog`]: our *local* usage samples newer
/// than `since`. Gives `remote_usage/` the repair path dialog always had — a
/// peer that lost its copy asks again and refills it, instead of waiting for
/// this device to restart and re-advertise from scratch.
async fn get_usage(State(app): State<AppHandle>, Query(q): Query<UsageQuery>) -> Result<Json<Vec<UsageHistoryRecord>>, StatusCode> {
    let records = app.try_state::<UsageHistoryStore>().map(|s| s.read_all()).unwrap_or_default();
    Ok(Json(usage_since(&records, q.since)))
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    since: u64,
}

/// Cap on one token-range response. The peer re-asks from its new watermark on
/// the next push, so a large backlog drains over several cycles instead of one
/// multi-megabyte body.
const MAX_TOKEN_RANGE: usize = 5_000;

/// Catch-up endpoint: a peer asks for our *local* token records above the `seq`
/// it already holds for us.
async fn get_tokens(State(app): State<AppHandle>, Query(q): Query<TokenQuery>) -> Result<Json<Vec<crate::token_history::TokenRecord>>, StatusCode> {
    let records = app
        .try_state::<crate::token_history::TokenHistoryStore>()
        .map(|s| s.records_since_seq(q.since, MAX_TOKEN_RANGE))
        .unwrap_or_default();
    Ok(Json(records))
}

/// How long the sending dashboard waits on the hop.
///
/// Its own constant rather than the pusher's 10 s: that one is tuned for
/// fire-and-forget heartbeats where a miss costs a retry nobody waits for, while
/// this is a synchronous wait a user is sitting through, and its expiry produces
/// [`Outcome::Unknown`] — a genuinely worse answer than a slow one.
pub(crate) const MESSAGE_HOP_TIMEOUT_SECS: u64 = 20;

/// Deliver one relayed message into a local session's inbox.
///
/// The whole point of the architecture is concentrated here: this runs as the
/// user who owns the target session, on the machine that owns it, so it is the
/// only party entitled to read that session's messaging key — and no credential
/// ever crossed the wire to get here. The sender told us a project name and some
/// text; everything else (does the session exist, where does it listen, which
/// key authenticates to it) is answered locally, from our own registry.
///
/// Auth and source scope are already settled by [`guard`].
async fn post_message(
    State(app): State<AppHandle>,
    // The peer's real source address. `origin_device` is a field the *sender*
    // chooses, so logging it alone lets anyone holding the token attribute a
    // send to the user's own laptop. This is the one route that starts a turn
    // inside a live agent; its success path owes the audit something the sender
    // cannot pick.
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(env): Json<MessageEnvelope>,
) -> (StatusCode, Json<Receipt>) {
    let receipt = |o: Outcome| Receipt::new(o, &env.message_id, &env.target_project, Some(&env.origin_device));
    let refuse = |reason: &str, status: StatusCode, detail: Option<String>| {
        let mut r = receipt(Outcome::Refused).because(reason);
        if let Some(d) = detail {
            r = r.detailed(d);
        }
        tracing::warn!(
            chat_id = %env.target_project,
            decision = "peer_refused",
            peer_ip = %peer.ip(),
            origin_device = %env.origin_device,
            message_id = %env.message_id,
            reason,
            "relayed message refused"
        );
        (status, Json(r))
    };

    // The opt-in, checked before anything else. Passing the guard proves the
    // caller holds the sync token and comes from an allowed source — it does not
    // prove this machine's owner agreed that peers may start turns in their
    // agents. `listen` bought a read-only view of session state; this is a
    // different grant and needs its own yes.
    if !app.try_state::<ConfigState>().is_some_and(|c| c.snapshot().sync.accept_messages) {
        return refuse(
            "messages_not_accepted",
            StatusCode::FORBIDDEN,
            Some("this device has sync.accept_messages off, so it accepts state pushes but not relayed messages".into()),
        );
    }

    if env.origin_device.is_empty() || env.message_id.is_empty() || env.target_project.is_empty() {
        return refuse("malformed_envelope", StatusCode::BAD_REQUEST, Some("origin_device, message_id and target_project are all required".into()));
    }
    if env.text.trim().is_empty() {
        // The receiver ignores a frame whose content is empty, without a word.
        // Writing one anyway would report `written` for a message no agent will
        // ever see — the exact over-claim this feature's vocabulary exists to
        // prevent.
        return refuse("empty_text", StatusCode::BAD_REQUEST, None);
    }
    if env.text.len() > crate::peer_message::MAX_TEXT_BYTES {
        return refuse("too_large", StatusCode::PAYLOAD_TOO_LARGE, Some(format!("{} bytes, cap is {}", env.text.len(), crate::peer_message::MAX_TEXT_BYTES)));
    }

    let (Some(cfg_state), Some(registry), Some(dedupe)) = (
        app.try_state::<ConfigState>(),
        app.try_state::<crate::session_registry::SessionRegistry>(),
        app.try_state::<MessageDedupe>(),
    ) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(receipt(Outcome::Refused).because("state_unavailable")));
    };
    let cfg = cfg_state.snapshot();
    let now = now_ms();

    // Ask Tailscale which machine this actually came from, rather than believing
    // `origin_device`. The token cannot answer this — it is one shared secret
    // for the fleet, so it proves a token-holder and not a machine.
    //
    // Only a *contradicted* claim refuses. No answer, or no configured binding,
    // is `Claimed`: that is today's behaviour, and refusing there would break
    // every deployment that has not written a `peer_identity` map yet — which,
    // on the day this ships, is all of them.
    let (attestation, peer_id) = match app.try_state::<crate::tailnet::TailnetResolver>() {
        Some(r) => r.attest_peer(peer.ip(), &env.origin_device, &cfg.sync.peer_identity),
        None => (crate::tailnet::Attestation::Claimed, None),
    };
    // Two tiers, and the difference matters exactly once: `attestation` decides
    // whose rows a push becomes and tolerates the unbound happy coincidence,
    // while anything that *authorises* — starting a process, or disclosing this
    // machine's directories so one can be approved — requires a binding this
    // receiver wrote down. Without that split a node holding the fleet token
    // attests itself by truthfully naming itself, which is exactly the
    // circularity `peer_identity` exists to break.
    let sender_is_bound = app
        .try_state::<crate::tailnet::TailnetResolver>()
        .is_some_and(|r| r.peer_is_bound(peer.ip(), &env.origin_device, &cfg.sync.peer_identity));
    if attestation == crate::tailnet::Attestation::Mismatch {
        return refuse(
            "device_mismatch",
            StatusCode::FORBIDDEN,
            Some(format!(
                "claimed device \"{}\" is bound to a different Tailscale node than the one this connection came from",
                env.origin_device
            )),
        );
    }

    let mut inbox = registry.inbox_for(&env.target_project, cfg.projects_root.as_deref(), now);

    // A project with nothing running is where this route used to end. If its
    // owner listed it in `auto_start.json`, open a real terminal session for it
    // instead and deliver into that.
    //
    // Deliberately only from `NotFound`. `Ambiguous` already has two sessions
    // and `NoInbox` has one; adding a third would make either worse, and
    // `Unreadable` is the one state where we do not know whether a session
    // exists — starting on a failed look is how a machine ends up with two
    // agents in one directory.
    let mut started_session = false;
    // `listed_dir` runs before the attestation check, and the order matters:
    // asked about a project nobody ever listed, the honest answer is that
    // nothing is running for it, not that the caller failed a check for a start
    // it never requested. Gating on attestation first would turn every
    // dead-project reply into `403 start_unattested` for a `Claimed` peer — the
    // normal state whenever whois cannot answer — the moment a single project
    // was listed.
    let startable = app.try_state::<crate::auto_start_store::AutoStartStore>().map(|s| s.snapshot()).unwrap_or_default();
    let listed = crate::session_launcher::listed_dir(&env.target_project, &startable, cfg.projects_root.as_deref()).is_ok();

    // Not listed, but this machine has directories that would derive the id: say
    // so, and offer them. That answer is what lets the *sender's* user approve
    // one — the prompt has to be raised where a human plausibly is, and only
    // this machine can name its own directories.
    //
    // Attestation is required to receive them even though nothing is started:
    // the list is a small disclosure of this machine's layout, and an
    // unattested peer gets the plain absence instead. It is checked here rather
    // than above the whole block so that a project nobody ever listed still
    // answers `no_such_session` — the honest reply — instead of failing a check
    // for a start it never asked for.
    if matches!(inbox, InboxLookup::NotFound) && !listed && sender_is_bound {
        let candidates = crate::session_launcher::candidates_for(&env.target_project, cfg.projects_root.as_deref());
        if !candidates.is_empty() {
            let r = receipt(Outcome::Refused)
                .because("start_not_listed")
                .detailed("nothing is running for that project and it is not listed as startable here; its owner can approve one of the offered directories")
                .with_candidates(candidates);
            tracing::info!(
                chat_id = %env.target_project,
                decision = "peer_refused",
                peer_ip = %peer.ip(),
                origin_device = %env.origin_device,
                message_id = %env.message_id,
                reason = "start_not_listed",
                candidates = r.start_candidates.len(),
                "relayed message refused, offering directories its owner could approve"
            );
            return (receipt_status(&r), Json(r));
        }
    }

    if matches!(inbox, InboxLookup::NotFound) && listed {
        // Starting a process on a peer's word needs the stronger half of the
        // identity check. `Claimed` is the fail-open answer — no whois reply, or
        // no binding configured — which is tolerable for writing into an agent
        // the user chose to run, and is not tolerable for causing one to exist.
        if !sender_is_bound {
            return refuse(
                "start_unattested",
                StatusCode::FORBIDDEN,
                Some("starting a session requires a sync.peer_identity entry on this device binding the sending device name to its Tailscale node; without one the name is only claimed".into()),
            );
        }
        let Some(guard) = app.try_state::<crate::session_launcher::StartGuard>() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(receipt(Outcome::Refused).because("state_unavailable")));
        };
        match crate::session_launcher::start_and_wait(&env.target_project, &startable, cfg.projects_root.as_deref(), &registry, &guard, now).await {
            crate::session_launcher::StartResult::Refused(refusal) => {
                let r = receipt(Outcome::Refused).because(refusal.slug()).detailed(refusal.detail());
                // The status comes from the canonical map rather than being
                // chosen here, so the handler and `receipt_status` cannot
                // disagree about what the same slug means.
                return refuse(refusal.slug(), receipt_status(&r), Some(refusal.detail().into()));
            }
            crate::session_launcher::StartResult::Settled { inbox: settled, started } => {
                inbox = settled;
                started_session = started;
                if started {
                    tracing::info!(
                        chat_id = %env.target_project,
                        decision = "peer_start",
                        peer_ip = %peer.ip(),
                        origin_device = %env.origin_device,
                        message_id = %env.message_id,
                        reached_inbox = !matches!(inbox, InboxLookup::NotFound),
                        "started a terminal session for a project with none"
                    );
                }
            }
        }
    }

    let (pid, socket_path) = match inbox {
        InboxLookup::Found { pid, socket_path } => (pid, socket_path),
        InboxLookup::Ambiguous { sessions } => {
            return refuse("ambiguous_target", StatusCode::CONFLICT, Some(format!("{sessions} live interactive sessions share that directory; picking one would decide which agent reads this")));
        }
        // Reached either because the feature is off for this project, or
        // because a session we started had not published its inbox in time. The
        // second is a different fact and says so: a terminal is now open on that
        // machine, and the message is still not delivered.
        //
        // `Unreachable`, not `Refused` — we did not decline, we tried and found
        // nothing listening yet — and answered `200` for the same reason the
        // `NoInbox` arm below is: the body carries the real verdict, and a
        // sender that reads the status first would otherwise turn it into a
        // transport diagnosis.
        InboxLookup::NotFound if started_session => {
            let r = receipt(Outcome::Unreachable)
                .because("start_not_ready")
                .detailed("a session was launched for that project and had not registered within the start window, so nothing was written; whether it comes up is not observable from here");
            tracing::info!(
                chat_id = %env.target_project,
                decision = "peer_write",
                peer_ip = %peer.ip(),
                origin_device = %env.origin_device,
                message_id = %env.message_id,
                outcome = "unreachable",
                reason = "start_not_ready",
                "relayed message not written after starting a session"
            );
            return (StatusCode::OK, Json(r));
        }
        InboxLookup::NotFound => return refuse("no_such_session", StatusCode::NOT_FOUND, Some("no live interactive session on this machine derives that project id".into())),
        InboxLookup::Unreadable => return refuse("registry_unreadable", StatusCode::SERVICE_UNAVAILABLE, Some("this machine's session registry could not be read".into())),
        InboxLookup::NoInbox => {
            let r = receipt(Outcome::Unreachable).because("no_inbox").detailed("the session is live but publishes no messaging socket");
            tracing::info!(chat_id = %env.target_project, decision = "peer_write", peer_ip = %peer.ip(), origin_device = %env.origin_device, message_id = %env.message_id, outcome = "unreachable", reason = "no_inbox", "relayed message not written");
            return (StatusCode::OK, Json(r));
        }
    };

    // Claimed before the write so two hops racing one retry cannot both write;
    // released below if the write then fails, because the claim means "we
    // already wrote this id" and a failed write did not.
    if !dedupe.claim(&env.origin_device, &env.message_id, now) {
        tracing::info!(chat_id = %env.target_project, decision = "peer_write", peer_ip = %peer.ip(), origin_device = %env.origin_device, message_id = %env.message_id, outcome = "duplicate", "relayed message already written under this id");
        return (StatusCode::OK, Json(receipt(Outcome::Duplicate)));
    }

    // The reply port is *this* machine's dashboard: a reply is POSTed by the
    // receiving agent to its own loopback, which is here.
    let content = build_content(&Relayed {
        origin_device: &env.origin_device,
        from_agent: &env.from_agent,
        from_label: env.from_label.as_deref(),
        text: &env.text,
        reply_to: env.reply_to.as_deref(),
        message_id: &env.message_id,
        in_reply_to: env.in_reply_to.as_deref(),
        reply_port: cfg.server_port,
        attestation,
        tailnet_user: peer_id.as_ref().and_then(|p| p.user.as_deref()),
    });
    let from = from_id(&env.origin_device, &env.from_agent);
    // Connecting, reading the key file and writing all block. Off the async
    // workers rather than accepted inline (as `get_agents`' registry read is):
    // that one is bounded by a 5 s cache, while a message write opens a socket
    // whose peer may be a busy pipe, and its own write timeout is 5 s.
    let message_id = env.message_id.clone();
    let written = tokio::task::spawn_blocking(move || deliver_to_inbox(&socket_path, pid, &message_id, &from, &content)).await;
    let written = match written {
        Ok(result) => result,
        Err(e) => Err(crate::peer_message::InboxError::WriteFailed(format!("the write task did not finish: {e}"))),
    };
    match written {
        Ok(report) => {
            tracing::info!(
                chat_id = %env.target_project,
                decision = "peer_write",
            peer_ip = %peer.ip(),
                origin_device = %env.origin_device,
                message_id = %env.message_id,
                pid,
                bytes = report.bytes,
                authenticated = report.authenticated,
                outcome = "written",
                "frame written to the session's inbox"
            );
            (StatusCode::OK, Json(receipt(Outcome::Written)))
        }
        Err(e) => {
            dedupe.release(&env.origin_device, &env.message_id);
            let detail = crate::peer_message::redact(&e.detail());
            tracing::warn!(chat_id = %env.target_project, decision = "peer_write", peer_ip = %peer.ip(), origin_device = %env.origin_device, message_id = %env.message_id, pid, outcome = "unreachable", detail = %detail, "frame not written");
            (StatusCode::OK, Json(receipt(Outcome::Unreachable).because("inbox_dead").detailed(detail)))
        }
    }
}

/// Which outcome a failed hop request is, given only what `reqwest` can tell us.
///
/// The distinction that matters is between "nothing was written" and "we cannot
/// know". A refused connection is the first: the peer never received the
/// request, so the frame certainly did not reach a socket. A timeout is the
/// second — the peer may have written the frame and lost the answer on the way
/// back, which is the repo's existing `SendError::maybe_delivered` shape. Every
/// other transport failure (a connection broken mid-response, a body that never
/// finished) is the same "sent, answer lost" position and is treated as such;
/// guessing the safer-sounding `Unreachable` there would invite a blind retry
/// that delivers twice.
fn hop_failure_outcome(is_connect: bool, is_timeout: bool) -> Outcome {
    if is_connect && !is_timeout {
        Outcome::Unreachable
    } else {
        Outcome::Unknown
    }
}

/// The HTTP status the *sender's* loopback route returns for a receipt.
///
/// `Unknown` is deliberately `200`, not a 5xx: a 5xx reads as "it failed, retry",
/// and retrying a message that may already have been written is how one message
/// becomes two. The body says what is and is not known; the status must not
/// contradict it.
///
/// A refusal maps through its `reason` so the peer's own status survives the
/// relay. Flattening every relayed refusal to `400` would tell a caller "your
/// request was malformed" when the peer actually said "no such session over
/// here" — a different problem with a different fix, and the caller only ever
/// sees this leg.
pub fn receipt_status(receipt: &Receipt) -> StatusCode {
    match receipt.outcome {
        Outcome::Written | Outcome::Duplicate | Outcome::Unknown => StatusCode::OK,
        Outcome::Unreachable => StatusCode::BAD_GATEWAY,
        Outcome::Refused => match receipt.reason.as_deref() {
            Some("no_such_session") => StatusCode::NOT_FOUND,
            Some("ambiguous_target") => StatusCode::CONFLICT,
            Some("too_large") => StatusCode::PAYLOAD_TOO_LARGE,
            Some("registry_unreadable") | Some("no_sync_token") => StatusCode::SERVICE_UNAVAILABLE,
            Some("state_unavailable") => StatusCode::INTERNAL_SERVER_ERROR,
            // The auto-start refusals, grouped by what the caller can do about
            // them rather than by which check produced them. Without these arms
            // every one falls to `400`, which tells a caller its request was
            // malformed when the truth is that the target is absent, the machine
            // declined, or it should try again in a moment.
            Some("start_not_listed") | Some("start_path_mismatch") | Some("start_no_directory") => StatusCode::NOT_FOUND,
            Some("start_unattested") | Some("messages_not_accepted") => StatusCode::FORBIDDEN,
            Some("start_already_running") => StatusCode::CONFLICT,
            Some("start_untrusted_directory") | Some("start_no_launcher") | Some("start_not_realized") => StatusCode::SERVICE_UNAVAILABLE,
            // A local start: the session now exists, and the relay was still
            // declined — the same answer as `local_target`, and the same status,
            // because what the caller asked for is what did not happen. The
            // detail says what did.
            Some("local_target_started") => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_REQUEST,
        },
    }
}

/// What a peer's HTTP answer amounts to, given its raw body.
///
/// Split out from [`send_message_hop`] so the precedence below is testable
/// without a live peer.
#[derive(Debug, PartialEq)]
enum HopAnswer {
    /// The peer sent a receipt. Its judgement stands; the caller restamps the
    /// address fields and returns it.
    Peer(Receipt),
    /// No receipt we could read, so the status is all there is to go on.
    Status(Outcome, &'static str, String),
}

/// Judge a peer's answer, **body first**.
///
/// A readable receipt wins over the status code no matter what that code is,
/// because [`receipt_status`] derives the status *from* the receipt: the peer's
/// `no_such_session` is a `404` on purpose, so a status-first reading turns
/// "nothing is running under that name over here" into "your peer is too old to
/// have this route" — a different problem, pointing at a redeploy that will not
/// help. That misreading happened in production (2026-08-31) and is what this
/// ordering exists to prevent. The mirrored cases are `409 ambiguous_target`,
/// `413 too_large` and `503 registry_unreadable`, of which the first was also
/// classed `Unreachable` rather than `Refused` — the wrong *outcome*, not just
/// the wrong reason.
///
/// The status map remains for answers with no receipt in them, which is exactly
/// the set the handler never produced: [`guard`] rejects with a bare status and
/// no body, and a peer predating the route 404s out of axum's own fallback. Left
/// to the unreadable-body arm those would read `unknown` — "may or may not have
/// been written" — when we know with certainty nothing was. This feature is
/// built so it cannot over-claim delivery, and manufacturing doubt where there
/// is none is the same defect mirrored; it would also hide the two failures
/// every rollout passes through, a mistyped `sync.token` and a peer without the
/// route, behind a permanent `200 unknown`.
/// What a peer's bare status means when it carried no receipt.
///
/// Shared by both hops because they must agree: the same peer condition — an
/// unreachable route, a mistyped token — has to be reported identically whether
/// it was a message or a grant that hit it, and two copies of a five-line map
/// are exactly the shape that drifts.
fn hop_status_reason(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "peer_rejected_token",
        403 => "peer_refused_source",
        404 | 405 => "peer_lacks_route",
        _ => "peer_error",
    }
}

fn hop_answer(status: StatusCode, body: &[u8]) -> HopAnswer {
    match serde_json::from_slice::<Receipt>(body) {
        Ok(peer_receipt) => HopAnswer::Peer(peer_receipt),
        Err(e) if status.is_success() => {
            // A body we could not read is the same position as a lost response:
            // the peer acted, we did not learn how.
            HopAnswer::Status(Outcome::Unknown, "unreadable_receipt", format!("peer answered {status} with a body this build could not read: {e}"))
        }
        Err(_) => {
            let reason = hop_status_reason(status);
            // Everything but `peer_error` is a refusal we know landed nowhere;
            // an unclassified status is the one where "we could not reach it"
            // is the honest reading.
            let outcome = if reason == "peer_error" { Outcome::Unreachable } else { Outcome::Refused };
            HopAnswer::Status(outcome, reason, format!("peer answered {status} with no receipt"))
        }
    }
}

/// Make the dashboard-to-dashboard hop and relay whatever the peer observed.
///
/// The peer's *judgement* is passed through untouched when there is one: it is
/// the only party that saw the socket, so re-deriving an outcome here would be
/// this dashboard asserting something it did not witness. Only the transport
/// failures — where no receipt exists — are judged locally, by
/// [`hop_failure_outcome`] and [`hop_answer`].
///
/// The address fields are re-stamped, though, and that is not the same thing.
/// The peer answers about the project id *it* resolved and stamps the device it
/// heard from — its own view, correct from where it stands and backwards from
/// the caller's. `target` and `device` here are the caller's own words, so the
/// receipt echoes the address they typed rather than the peer's half of it.
pub async fn send_message_hop(app: &AppHandle, origin_addr: &str, env: &MessageEnvelope, target: &str, device: &str) -> Receipt {
    let receipt = |o: Outcome| Receipt::new(o, &env.message_id, target, Some(device));
    let Some(token) = app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token).filter(|t| !t.is_empty()) else {
        return receipt(Outcome::Refused).because("no_sync_token").detailed("this device has no sync token, so it cannot authenticate to a peer");
    };
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(MESSAGE_HOP_TIMEOUT_SECS)).build() {
        Ok(c) => c,
        Err(e) => return receipt(Outcome::Refused).because("client_build_failed").detailed(e.to_string()),
    };
    let url = format!("{}/api/sync/message", origin_addr.trim_end_matches('/'));
    let started = now_ms();
    let result = client.post(&url).bearer_auth(&token).json(env).send().await;
    let elapsed_ms = now_ms() - started;
    match result {
        Ok(resp) => {
            let status = resp.status();
            tracing::debug!(peer = %origin_addr, decision = "peer_relay", message_id = %env.message_id, status = %status, elapsed_ms, "message hop answered");
            let body = resp.bytes().await.unwrap_or_default();
            match hop_answer(status, &body) {
                // The peer observed the socket; its judgement stands, restamped
                // with the caller's own address.
                HopAnswer::Peer(peer_receipt) => Receipt {
                    message_id: env.message_id.clone(),
                    target: target.to_string(),
                    device: Some(device.to_string()),
                    ..peer_receipt
                },
                HopAnswer::Status(outcome, reason, detail) => receipt(outcome).because(reason).detailed(detail),
            }
        }
        Err(e) => {
            let outcome = hop_failure_outcome(e.is_connect(), e.is_timeout());
            tracing::debug!(peer = %origin_addr, decision = "peer_relay", message_id = %env.message_id, elapsed_ms, error = %e, outcome = ?outcome, "message hop failed");
            let reason = if outcome == Outcome::Unreachable { "peer_unreachable" } else { "response_lost" };
            receipt(outcome).because(reason).detailed(crate::peer_message::redact(&e.to_string()))
        }
    }
}

/// One machine telling another that its user approved starting a project.
///
/// It rides the same authenticated, source-scoped channel as everything else
/// here, and it is the only thing in this crate that writes a **standing
/// permission** on another computer — every other route acts once and is done.
/// So it is the one place where "who asked" has to be more than a claim, and
/// `post_grant` refuses anything short of [`crate::tailnet::Attestation::Attested`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantEnvelope {
    pub origin_device: String,
    /// The chat_id as the *receiving* machine derives it — the same string
    /// `inbox_for` compares and a message addresses.
    pub project: String,
    /// The absolute directory on the receiving machine. Chosen by the user from
    /// candidates that machine itself supplied, never composed by the sender.
    pub dir: String,
}

/// What the receiver made of a grant. Deliberately not a [`Receipt`]: that type
/// answers "what happened to my message", and every one of its fields would be
/// empty or a lie here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantReceipt {
    pub granted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl GrantReceipt {
    fn refused(reason: &str, detail: impl Into<String>) -> Self {
        Self { granted: false, reason: Some(reason.to_string()), detail: Some(detail.into()) }
    }
}

/// Record a peer's user-approved permission to start a project here.
///
/// The approval was given on the *other* machine, which is the only place a
/// human was, and that is the one fact this route takes on faith. Everything
/// else is re-established locally: the directory is checked here, against this
/// machine's filesystem and this machine's Claude Code trust state, by exactly
/// the function that will gate the start itself. A peer cannot grant anything
/// this dashboard would not have granted about its own disk.
async fn post_grant(State(app): State<AppHandle>, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(env): Json<GrantEnvelope>) -> (StatusCode, Json<GrantReceipt>) {
    let refuse = |reason: &str, status: StatusCode, detail: String| {
        tracing::warn!(chat_id = %env.project, decision = "peer_refused", peer_ip = %peer.ip(), origin_device = %env.origin_device, reason, "grant refused");
        (status, Json(GrantReceipt::refused(reason, detail)))
    };

    let (Some(cfg_state), Some(store)) = (app.try_state::<ConfigState>(), app.try_state::<crate::auto_start_store::AutoStartStore>()) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(GrantReceipt::refused("state_unavailable", "the dashboard is still starting")));
    };
    let cfg = cfg_state.snapshot();
    if !cfg.sync.accept_messages {
        return refuse("messages_not_accepted", StatusCode::FORBIDDEN, "this device has sync.accept_messages off".into());
    }
    if env.project.trim().is_empty() || env.dir.trim().is_empty() {
        return refuse("malformed_grant", StatusCode::BAD_REQUEST, "project and dir are both required".into());
    }

    // Bound, not merely attested. It is not enough that the claimed name
    // *happens* to equal the node's own, which a sender arranges for free by
    // telling the truth about itself. A message writes into an agent the user
    // already chose to run; this writes a permission that outlives the message,
    // the session and the reboot, so the fail-open answer is not good enough.
    let bound = app
        .try_state::<crate::tailnet::TailnetResolver>()
        .is_some_and(|r| r.peer_is_bound(peer.ip(), &env.origin_device, &cfg.sync.peer_identity));
    if !bound {
        return refuse(
            "grant_unattested",
            StatusCode::FORBIDDEN,
            "a standing permission is only accepted from a device this machine has bound to a Tailscale node in sync.peer_identity".into(),
        );
    }

    // The pair is validated as if it were already in the list, by the same
    // function that will gate the start — so a grant can never record something
    // `check_startable` would later refuse, and the two cannot drift.
    let candidate: std::collections::BTreeMap<String, String> = [(env.project.clone(), env.dir.clone())].into_iter().collect();
    if let Err(refusal) = crate::session_launcher::check_startable(&env.project, &candidate, cfg.projects_root.as_deref()) {
        let r = GrantReceipt::refused(refusal.slug(), refusal.detail());
        tracing::warn!(chat_id = %env.project, decision = "peer_refused", peer_ip = %peer.ip(), origin_device = %env.origin_device, reason = refusal.slug(), "grant refused");
        return (StatusCode::BAD_REQUEST, Json(r));
    }

    let changed = store.grant(&env.project, env.dir.trim());
    tracing::info!(
        chat_id = %env.project,
        decision = "peer_grant",
        peer_ip = %peer.ip(),
        origin_device = %env.origin_device,
        dir = %env.dir,
        changed,
        "recorded a user-approved permission to start this project"
    );
    (StatusCode::OK, Json(GrantReceipt { granted: true, reason: None, detail: None }))
}

/// Ask a peer to record a grant its user approved here.
pub async fn send_grant_hop(app: &AppHandle, origin_addr: &str, env: &GrantEnvelope) -> GrantReceipt {
    let Some(token) = app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token).filter(|t| !t.is_empty()) else {
        return GrantReceipt::refused("no_sync_token", "this device has no sync token, so it cannot authenticate to a peer");
    };
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(MESSAGE_HOP_TIMEOUT_SECS)).build() {
        Ok(c) => c,
        Err(e) => return GrantReceipt::refused("client_build_failed", e.to_string()),
    };
    let url = format!("{}/api/sync/grant", origin_addr.trim_end_matches('/'));
    match client.post(&url).bearer_auth(&token).json(env).send().await {
        Ok(resp) => {
            let status = resp.status();
            // Body first, for the reason `hop_answer` documents: the peer's own
            // verdict is more specific than any status code, and reading the
            // status first is how a real refusal became a wrong diagnosis.
            match resp.json::<GrantReceipt>().await {
                Ok(receipt) => receipt,
                // Same reasoning as `hop_answer`: the answers this handler never
                // produced — the guard's bare 401/403, and axum's 404 for a peer
                // without the route — are certain refusals, and reporting them as
                // an unreadable body would hide the two failures every rollout
                // passes through behind a vague one.
                Err(_) if !status.is_success() => GrantReceipt::refused(hop_status_reason(status), format!("peer answered {status} with no receipt")),
                Err(_) => GrantReceipt::refused("unreadable_receipt", format!("peer answered {status} with a body this build could not read")),
            }
        }
        Err(e) => GrantReceipt::refused("peer_unreachable", crate::peer_message::redact(&e.to_string())),
    }
}

/// Whether a sync listener is actually bound and serving right now.
///
/// Deliberately *not* re-derived from config by its readers. The config
/// predicate and the running listener disagree in three reachable ways: an
/// empty-string token passes `token.is_some()` while `lib.rs` refuses to start
/// on it, `sync.listen` hot-reloads through `config_watcher` while the listener
/// is start-only, and the bind below can simply fail on a taken port. Every one
/// of those makes config say "listening" while nothing is bound — and
/// `/api/agents` reports this to a caller that reads an empty `peers` as "the
/// other machine has nothing", which is the exact over-claim that route exists
/// to avoid. So the flag records the outcome, set at the one place that knows it.
///
/// It means **bound at startup, still serving** — not *reachable*. A narrow bind
/// introduces one gap it cannot see: if the Tailscale address it bound goes away
/// (logout, node-key reset, tailnet switch), the socket stays bound to an address
/// that no longer exists, this flag stays `true`, and every peer gets connection
/// refused until the app restarts. Under the old wildcard bind that could not
/// happen. `spawn_reaper` re-checks the bound set against the live tailnet
/// addresses and warns on divergence, which is the observable half; there is
/// deliberately no rebind (see `run_listener`).
#[derive(Debug, Default)]
pub struct SyncListening(pub AtomicBool);

impl SyncListening {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn set(app: &AppHandle, v: bool) {
        if let Some(f) = app.try_state::<SyncListening>() {
            f.0.store(v, Ordering::Relaxed);
        }
    }
}

/// The addresses one listener run will bind, and whether that set is the
/// intended one. `degraded` is carried rather than re-derived by the caller so
/// the warning cannot be forgotten: the only way to learn the bind is wide is
/// to read the flag that also drives the log line.
#[derive(Debug, PartialEq, Eq)]
struct BindPlan {
    addrs: Vec<SocketAddr>,
    degraded: bool,
}

/// Pure half of the bind decision: given the local addresses we discovered,
/// which sockets should this run open?
///
/// Under `Tailnet` the tailnet addresses are bound *plus* both loopbacks —
/// binding a specific address does not serve 127.0.0.1, and the documented
/// localhost observer-peer harness pushes there. Several sockets rather than one
/// wildcard is the price of the narrowing; they share one router, so there is
/// one code path behind all of them.
///
/// Non-tailnet candidates are dropped rather than bound: the route lookup that
/// produces them answers with the LAN address when Tailscale is down, and
/// binding that would be the opposite of the intent — it would serve the LAN
/// and *not* the tailnet.
///
/// With no tailnet address the plan is the wildcard, flagged degraded. Refusing
/// to bind was rejected outright: it turns "the VPN came up a few seconds late"
/// into "sync is dead until the next restart", silently, which is the one
/// failure this whole change must not introduce.
fn select_binds(scope: SyncBindScope, candidates: &[IpAddr], port: u16) -> BindPlan {
    if scope == SyncBindScope::Any {
        return BindPlan { addrs: vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))], degraded: false };
    }
    let tailnet: Vec<IpAddr> = candidates.iter().copied().filter(|ip| is_tailnet(*ip)).collect();
    if tailnet.is_empty() {
        return BindPlan { addrs: vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))], degraded: true };
    }
    let mut addrs: Vec<SocketAddr> = tailnet.into_iter().map(|ip| SocketAddr::new(ip, port)).collect();
    addrs.push(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    addrs.push(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
    BindPlan { addrs, degraded: false }
}

/// This device's Tailscale addresses, or empty when the tailnet is not up.
///
/// Found by asking the routing table, not by enumerating interfaces: a
/// connect(2) on an *unconnected UDP socket* sends no packet — it only resolves
/// the route and pins the source address, which `local_addr` then reports. Six
/// lines of `std`, identical on both platforms, against `getifaddrs` on macOS
/// plus the two-pass `GetAdaptersAddresses` linked-list walk on Windows, or a
/// new interface-enumeration crate. Shelling out to `tailscale ip` was rejected
/// for the same reason it always is: PATH-dependent, absent on some installs,
/// and a process spawn for one address.
///
/// The probe targets are inside the tailnet by construction (the MagicDNS
/// resolver's v4 address; the tailnet's own v6 prefix), so a hit is routed over
/// the tailnet interface. Every answer is still filtered through [`is_tailnet`]
/// by the caller: with Tailscale down the default route answers instead and the
/// lookup yields a LAN address, which must not be mistaken for a tailnet one.
fn local_tailnet_addrs() -> Vec<IpAddr> {
    fn route_source(bind: &str, probe: &str) -> Option<IpAddr> {
        let sock = std::net::UdpSocket::bind(bind).ok()?;
        sock.connect(probe).ok()?;
        Some(sock.local_addr().ok()?.ip())
    }
    // The v4 probe alone is not enough to conclude Tailscale is up. 100.64/10 is
    // the real CGNAT range, and carriers hand it out directly — Starlink, T-Mobile
    // Home Internet, most mobile hotspots. On such a network with Tailscale DOWN,
    // the probe returns the *LAN* address, `is_tailnet` accepts it, and we would
    // bind that LAN address while reporting a clean (non-degraded) narrow bind —
    // then accept every other host behind the same carrier NAT as a peer. So
    // cross-check against the route to the public internet: with a real tailnet
    // there is a separate route and the two sources differ; on carrier CGNAT they
    // are the same address, which means there is no tailnet here.
    //
    // An exit node makes them match too, and that degrades to a wide bind rather
    // than a wrong narrow one — the safe direction.
    let public_route = route_source("0.0.0.0:0", "1.1.1.1:53");
    [("0.0.0.0:0", "100.100.100.100:53"), ("[::]:0", "[fd7a:115c:a1e0::53]:53")]
        .into_iter()
        .filter_map(|(bind, probe)| route_source(bind, probe))
        .filter(|ip| is_tailnet(*ip))
        .filter(|ip| !(ip.is_ipv4() && public_route == Some(*ip)))
        .collect()
}

/// How long to wait for a tailnet address before giving up and binding wide.
/// The dashboard autostarts at login and so does Tailscale, so losing that race
/// is ordinary. A short grace turns most of those launches into a narrow bind;
/// past it we bind anyway, because sync being down is worse than a wide socket
/// the source check still guards. Sync is eventually consistent — peers retry
/// every heartbeat — so the wait itself costs a peer at most one cycle.
const BIND_DISCOVERY_ATTEMPTS: u32 = 5;
const BIND_DISCOVERY_INTERVAL_MS: u64 = 2_000;

/// Impure half: poll for a tailnet address across the grace window, then plan.
async fn resolve_binds(scope: SyncBindScope, port: u16) -> BindPlan {
    let mut plan = select_binds(scope, &local_tailnet_addrs(), port);
    let mut attempt = 1;
    while plan.degraded && attempt < BIND_DISCOVERY_ATTEMPTS {
        tracing::debug!(attempt, "no tailnet address yet — waiting for the VPN before binding sync");
        tokio::time::sleep(Duration::from_millis(BIND_DISCOVERY_INTERVAL_MS)).await;
        attempt += 1;
        plan = select_binds(scope, &local_tailnet_addrs(), port);
    }
    plan
}

/// State the [`guard`] layer needs: the app (for the current token) and the
/// scope the listener was started with. The scope is carried, not re-read from
/// config, so the gate cannot contradict the socket it is guarding —
/// `bind_scope` is start-only, and a hot-reload that widened only the check
/// would be a silent, invisible widening.
#[derive(Clone)]
struct GuardState {
    app: AppHandle,
    scope: SyncBindScope,
}

/// The single gate in front of every sync route: source scope first, bearer
/// token second, both before any handler runs.
///
/// One layer rather than a check per handler, replacing four copies that were
/// already a maintenance hazard.
///
/// **It is ordering, not construction, that keeps a new route guarded.** axum
/// applies a layer only to routes already registered: "additional routes added
/// after `layer` is called will not have the middleware added". So a `.route(...)`
/// appended *below* the `.layer(...)` call — the natural place to add one —
/// compiles, type-checks, passes every test here, and serves with no source check
/// and no token. Every route must be registered **above** the layer, and
/// `guard_covers_every_route` fails if one is not.
///
/// Source before token on purpose: an out-of-scope caller is refused without the
/// secret ever being compared, so it cannot even be probed from off the tailnet.
async fn guard(State(gs): State<GuardState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Result<Response, StatusCode> {
    if !source_allowed(gs.scope, peer.ip()) {
        log_reject(peer.ip(), "source", req.uri().path());
        return Err(StatusCode::FORBIDDEN);
    }
    let token = gs.app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token);
    if !bearer_ok(req.headers(), token.as_deref()) {
        log_reject(peer.ip(), "token", req.uri().path());
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Cooldown between logged rejections from one address, and the cap on how many
/// addresses are remembered. A misconfigured peer retries every push cycle and a
/// scanner far faster than that, so an unthrottled line per rejection would
/// bury the log — the same failure the `sync push cycle` breadcrumb had in the
/// other direction. The cap bounds the map under a scan sweeping source
/// addresses; hitting it clears the whole map, which at worst re-logs an address
/// once more than the cooldown asked for.
const REJECT_LOG_COOLDOWN_MS: i64 = 60_000;
const REJECT_LOG_CAP: usize = 256;

static REJECT_SEEN: LazyLock<Mutex<HashMap<IpAddr, i64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Pure throttle decision, so the "once per address per window" rule is
/// testable without a clock or a log sink.
fn reject_log_due(seen: &mut HashMap<IpAddr, i64>, ip: IpAddr, now: i64) -> bool {
    if let Some(last) = seen.get(&ip) {
        if now - *last < REJECT_LOG_COOLDOWN_MS {
            return false;
        }
    }
    if seen.len() >= REJECT_LOG_CAP {
        seen.clear();
    }
    seen.insert(ip, now);
    true
}

/// A rejected request, at `warn!` and throttled per source. Not `debug!` like a
/// failed push: an offline peer is routine, but a peer being refused is a
/// misconfiguration (wrong token, wrong network, a `bind_scope` that does not
/// match how the devices actually reach each other) that only the user can fix,
/// and "sync went quiet" is unanswerable without knowing *which* gate closed.
/// Never logs the token or any part of it.
fn log_reject(ip: IpAddr, gate: &'static str, path: &str) {
    let due = REJECT_SEEN.lock().map(|mut seen| reject_log_due(&mut seen, ip, now_ms())).unwrap_or(true);
    if due {
        tracing::warn!(peer = %ip, gate, path, "sync request rejected");
    }
}

/// Bind and serve the sync routes. The bind set is chosen once, here: there is
/// no rebind path, so a tailnet address that appears (or changes) after this
/// point is picked up on the next app start, and `bind_scope`/`listen_port` are
/// start-only for the same reason. Rebinding live was considered and rejected
/// for Stage 1 — it needs graceful shutdown plus a re-bind that can fail on a
/// port still held by lingering connections (Windows has no SO_REUSEADDR
/// equivalent here), trading a rare narrow-bind miss for a rarer but total
/// outage. The source check covers the interval either way.
pub async fn run_listener(app: AppHandle, port: u16, scope: SyncBindScope) {
    let plan = resolve_binds(scope, port).await;
    if plan.degraded {
        // The reliability escape hatch, made loud. Everything still works; the
        // socket is simply wider than asked for until the next restart.
        tracing::warn!(
            wanted = "tailnet",
            bound = %plan.addrs.first().map(|a| a.to_string()).unwrap_or_default(),
            "no Tailscale address found — sync listening on all interfaces; non-tailnet sources are still refused"
        );
    }

    // Record what we actually bound so the heartbeat can notice it vanishing.
    *bound_tailnet().lock().unwrap() =
        if plan.degraded { Vec::new() } else { plan.addrs.iter().map(|a| a.ip()).filter(|ip| is_tailnet(*ip)).collect() };

    let router = Router::new()
        .route("/api/sync", post(post_sync))
        .route("/api/sync/dialog", get(get_dialog))
        .route("/api/sync/usage", get(get_usage))
        .route("/api/sync/tokens", get(get_tokens))
        .route("/api/sync/message", post(post_message))
        .route("/api/sync/grant", post(post_grant))
        .layer(middleware::from_fn_with_state(GuardState { app: app.clone(), scope }, guard))
        .with_state(app.clone());

    // One task per bound address; they share the router, so all sockets run the
    // same gate and the same handlers. A single failure is tolerated (an IPv6
    // loopback on a host with IPv6 off, say) as long as something is serving.
    let mut serving = JoinSet::new();
    for addr in &plan.addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!(%addr, ?scope, degraded = plan.degraded, "sync listening");
                let router = router.clone();
                serving.spawn(async move {
                    if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
                        tracing::error!(error = %e, "sync serve ended");
                    }
                });
            }
            Err(e) => tracing::warn!(%addr, error = %e, "sync bind failed on one address"),
        }
    }
    if serving.is_empty() {
        tracing::error!(port, ?scope, "sync bind failed on every address — listener not started");
        SyncListening::set(&app, false);
        return;
    }
    SyncListening::set(&app, true);
    while serving.join_next().await.is_some() {}
    SyncListening::set(&app, false);
}

/// Local usage samples newer than `since`, ascending (the store already
/// returns them sorted). Pure, so the range selection is unit-testable.
fn usage_since(records: &[UsageHistoryRecord], since: i64) -> Vec<UsageHistoryRecord> {
    records.iter().filter(|r| r.ts > since).cloned().collect()
}

/// Build the one push a cycle sends a peer: a full metadata snapshot of every
/// local session with its dialog stripped down to a `dialog_tip`, plus the
/// newest local usage timestamp.
///
/// Stateless by construction — nothing here depends on which peer it is going
/// to or on anything that peer was sent before, so there is no per-peer
/// bookkeeping to go stale, no backlog to chunk, and a failed push costs
/// nothing but a retry. Content moves on the pull side, where the party that
/// knows what it is missing does the asking.
fn build_push(
    device_name: &str,
    listen_port: u16,
    sessions: &[AgentSession],
    usage_tip: i64,
    token_tip: u64,
    registry: Option<&[crate::session_registry::LiveSession]>,
) -> SyncPush {
    SyncPush {
        device_name: device_name.to_string(),
        listen_port,
        sessions: sessions
            .iter()
            .map(|s| {
                let dialog_tip = s.dialog.iter().map(|e| e.timestamp).max().unwrap_or(0);
                let mut meta = s.clone();
                meta.dialog = Vec::new();
                SessionSync { session: meta, dialog_tip }
            })
            .collect(),
        usage_tip,
        token_tip,
        // `None` stays `None` all the way to the peer's roster: an unreadable
        // registry must not arrive as an empty one, which would read as "that
        // machine has no live sessions" — the exact absence-that-was-never-
        // established this whole path exists to avoid.
        registry_sessions: registry.map(|regs| {
            regs.iter()
                .map(|s| RegistrySync {
                    chat_id: s.chat_id.clone(),
                    name: s.name.clone(),
                    activity: s.activity,
                    activity_age_ms: s.activity_age_ms,
                    sessions: s.sessions,
                })
                .collect()
        }),
    }
}

async fn push_all(app: &AppHandle, client: &reqwest::Client) {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return;
    };
    let cfg = cfg_state.snapshot();
    let Some(token) = cfg.sync.token else {
        return;
    };
    if cfg.sync.peers.is_empty() || token.is_empty() {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // Local sessions only — received remote sessions are never re-broadcast.
    let sessions = state.snapshot();
    let usage_tip = app
        .try_state::<UsageHistoryStore>()
        .and_then(|s| s.read_all().last().map(|r| r.ts))
        .unwrap_or(0);
    let token_tip = app
        .try_state::<crate::token_history::TokenHistoryStore>()
        .and_then(|s| s.newest_seq())
        .unwrap_or(0);
    // Same 5 s cache `/api/agents` and message delivery read, so a push costs no
    // extra directory read and the three cannot disagree about what is live.
    let registry = app
        .try_state::<crate::session_registry::SessionRegistry>()
        .and_then(|r| r.live_sessions(cfg.projects_root.as_deref(), now_ms()));
    let push = build_push(&cfg.sync.device_name, cfg.sync.listen_port, &sessions, usage_tip, token_tip, registry.as_deref());
    // Cycle breadcrumb: push cadence should never silently stop while peers
    // are configured — if the failure logs go quiet, this shows whether the
    // pusher loop itself is still alive.
    tracing::trace!(peers = cfg.sync.peers.len(), sessions = sessions.len(), "sync push cycle");
    for peer in &cfg.sync.peers {
        let url = format!("{}/api/sync", peer.trim_end_matches('/'));
        match client.post(&url).bearer_auth(&token).json(&push).send().await {
            Ok(resp) if resp.status().is_success() => {}
            // Offline peers are routine — log at debug, not warn. Nothing to
            // roll back: the next push carries the same snapshot and the
            // receiver re-derives what it needs from the tips.
            Ok(resp) => tracing::debug!(peer = %peer, status = %resp.status(), "sync push rejected"),
            Err(e) => tracing::debug!(peer = %peer, error = %e, "sync push failed"),
        }
    }
}

/// Debounced pusher + heartbeat. Spawned unconditionally; every cycle re-reads
/// config, so `peers`/`token`/`device_name` hot-reload and an unconfigured
/// sync block just no-ops.
pub fn spawn_pusher(app: AppHandle, dirty: Arc<Notify>) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        loop {
            tokio::select! {
                _ = dirty.notified() => {
                    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
                }
                _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)) => {}
            }
            push_all(&app, &client).await;
        }
    });
}

/// Drop remote devices that stopped pushing (closed laptop, network loss).
pub fn spawn_reaper(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        loop {
            interval.tick().await;
            let Some(state) = app.try_state::<AppState>() else {
                continue;
            };
            if state.reap_remote(now_ms(), REMOTE_TTL_MS) {
                emit_sessions_updated_remote(&app);
            }
            warn_if_bound_address_vanished();
        }
    });
}

/// Tailnet addresses this process actually bound, recorded so the heartbeat can
/// notice they stopped existing. Empty under `Any` / a degraded bind, where the
/// wildcard socket cannot go stale.
static BOUND_TAILNET: std::sync::OnceLock<std::sync::Mutex<Vec<IpAddr>>> = std::sync::OnceLock::new();

fn bound_tailnet() -> &'static std::sync::Mutex<Vec<IpAddr>> {
    BOUND_TAILNET.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// The one degradation a narrow bind adds and the socket itself cannot report:
/// the bound Tailscale address goes away (logout, node-key reset, tailnet
/// switch) and the listener keeps a socket on an address that no longer exists.
/// Peers then get connection-refused until restart while this side looks
/// healthy — `SyncListening` stays true, nothing errors, sync just goes quiet.
/// There is no rebind (see `run_listener`), so the fix here is purely making it
/// diagnosable from the log rather than leaving "sync stopped" unanswerable.
/// Latched so a persistent divergence logs once, not every heartbeat.
fn warn_if_bound_address_vanished() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let bound = bound_tailnet().lock().unwrap().clone();
    if bound.is_empty() {
        return;
    }
    let live = local_tailnet_addrs();
    let missing: Vec<_> = bound.iter().filter(|b| !live.contains(b)).collect();
    if missing.is_empty() {
        WARNED.store(false, Ordering::Relaxed);
    } else if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            gone = ?missing,
            live = ?live,
            "a bound tailnet address is no longer local — peers cannot reach this device until the app restarts"
        );
    }
}

/// Resolve a (possibly remote) session id into a catch-up fetch target:
/// the owning device, the raw id on the origin (prefix stripped), and the
/// origin address. `None` for local ids: no remote device prefix matches.
///
/// Longest prefix wins. A device name may contain a slash (it is a user-editable
/// config string), so with devices `win` and `win/box` present, the shortest
/// match on `win/box/transcripts` yields device `win` and a raw id
/// `box/transcripts` that exists on no machine — and the `BTreeMap` iteration
/// order made that the answer, deterministically. Same rule as
/// `peer_message::resolve_message_target`, which faces the identical collision.
fn resolve_fetch_target(
    remote: &std::collections::BTreeMap<String, RemoteDevice>,
    session_id: &str,
) -> Option<(String, String, String)> {
    remote
        .iter()
        .filter(|(d, _)| session_id.starts_with(&format!("{d}/")))
        .max_by_key(|(d, _)| d.len())
        .map(|(d, dev)| (d.clone(), session_id[d.len() + 1..].to_string(), dev.origin_addr.clone()))
}

/// Wire shape for the `history_loading` event the history window listens to.
#[derive(Serialize, Clone)]
struct HistoryLoading<'a> {
    id: &'a str,
    loading: bool,
}

fn emit_history_loading(app: &AppHandle, id: &str, loading: bool) {
    let _ = app.emit("history_loading", HistoryLoading { id, loading });
}

/// GET the origin's full dialog for one raw session id. `None` on any
/// failure (origin offline, auth mismatch, parse error) — all logged at
/// debug, since offline peers are routine.
/// Fetch one session's missing dialog range and merge it. Spawned from
/// `post_sync` whenever the origin's advertised tip is newer than what we hold.
///
/// Fire-and-forget: on failure nothing is lost, because we never recorded that
/// we had the range — the next push re-advertises the same tip and we ask
/// again. That is the property the push-delta model could not have, where a
/// range the receiver dropped was gone for the rest of the sender's run.
fn fetch_dialog_range(app: AppHandle, device: String, origin_addr: String, pull: DialogPull) {
    tauri::async_runtime::spawn(async move {
        let Some(token) = app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token) else {
            return;
        };
        let url = format!("{origin_addr}/api/sync/dialog");
        let query = [("id", pull.raw_id.clone()), ("since", pull.since.to_string())];
        let Some(entries): Option<Vec<DialogEntry>> = get_json(&url, &query, &token, "dialog pull").await else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let id = format!("{device}/{}", pull.raw_id);
        let merged = {
            let mut remote = state.remote.lock().unwrap();
            remote.get_mut(&device).and_then(|dev| dev.sessions.iter_mut().find(|s| s.id == id)).map(|s| {
                merge_dialog_entries(&mut s.dialog, &entries);
                s.clone()
            })
        };
        let Some(s) = merged else { return };
        tracing::debug!(session = %id, entries = entries.len(), since = pull.since, "dialog range pulled");
        if let Some(store) = app.try_state::<RemoteHistoryStore>() {
            store.save_device(&device, std::slice::from_ref(&s));
        }
        emit_sessions_updated_remote(&app);
    });
}

/// Usage counterpart of [`fetch_dialog_range`].
fn fetch_usage_range(app: AppHandle, device: String, origin_addr: String, since: i64) {
    tauri::async_runtime::spawn(async move {
        let Some(token) = app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token) else {
            return;
        };
        let url = format!("{origin_addr}/api/sync/usage");
        let query = [("since", since.to_string())];
        let Some(records): Option<Vec<UsageHistoryRecord>> = get_json(&url, &query, &token, "usage pull").await else {
            return;
        };
        if records.is_empty() {
            return;
        }
        if let Some(store) = app.try_state::<RemoteUsageStore>() {
            store.merge_device(&device, &records);
            tracing::debug!(device = %device, records = records.len(), since, "usage range pulled");
            // Nudge an open Work-intensity window to re-fetch the merged chart.
            emit_usage_limits_updated(&app);
        }
    });
}

/// Pull a peer's token records above `since` (its `seq`, not a timestamp) and
/// merge them. Mirrors [`fetch_usage_range`]; the `MAX_TOKEN_RANGE` cap bounds
/// one response, and the peer's next push re-advertises the same tip so a
/// truncated range is simply asked for again.
fn fetch_token_range(app: AppHandle, device: String, origin_addr: String, since: u64) {
    tauri::async_runtime::spawn(async move {
        let Some(token) = app.try_state::<ConfigState>().and_then(|c| c.snapshot().sync.token) else {
            return;
        };
        let url = format!("{origin_addr}/api/sync/tokens");
        let query = [("since", since.to_string())];
        let Some(records): Option<Vec<crate::token_history::TokenRecord>> = get_json(&url, &query, &token, "token pull").await else {
            return;
        };
        if records.is_empty() {
            return;
        }
        if let Some(store) = app.try_state::<crate::remote_tokens::RemoteTokenStore>() {
            let accepted = store.merge_device(&device, &records);
            tracing::debug!(device = %device, records = records.len(), accepted, since, "token range pulled");
            // Nudge an open Work-intensity window to re-fetch the merged chart.
            emit_usage_limits_updated(&app);
        }
    });
}

/// GET a bearer-authed JSON body from a peer; `None` on any failure — offline
/// origin, bad auth, malformed body. Every caller treats a miss the same way:
/// nothing is recorded as received, so the next push re-advertises the tip and
/// the range is simply asked for again. `what` names the operation for the log.
async fn get_json<T: serde::de::DeserializeOwned>(url: &str, query: &[(&str, String)], token: &str, what: &str) -> Option<T> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("reqwest client");
    match client.get(url).query(query).bearer_auth(token).send().await {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(url = %url, what, error = %e, "peer fetch parse failed");
                None
            }
        },
        Ok(r) => {
            tracing::debug!(url = %url, what, status = %r.status(), "peer fetch rejected");
            None
        }
        Err(e) => {
            tracing::debug!(url = %url, what, error = %e, "peer fetch failed");
            None
        }
    }
}


/// Catch-up fetch for one remote session's dialog, triggered when the history
/// window targets it. Always fetches the origin's full dialog, and the merge
/// dedups the overlap.
///
/// It deliberately asks for everything rather than for the range above our
/// newest held entry, which is what the routine [`fetch_dialog_range`] pull
/// does. The two differ in exactly one case: `merge_dialog_entries` can drop an
/// entry it cannot distinguish from a transcript re-read (a prompt repeated
/// with no reply between it and its twin), leaving our newest timestamp above
/// something we never stored — the one shape a `since` cannot express. Asking
/// whole is cheap at the single moment the dialog is actually read, so this
/// stays the belt-and-braces path behind the incremental one.
///
/// Brackets the fetch in `history_loading` events so the window can show a
/// hint. Fire-and-forget: on failure (origin offline) the window simply shows
/// whatever is held.
pub fn fetch_remote_dialog(app: AppHandle, session_id: String) {
    tauri::async_runtime::spawn(async move {
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let Some(cfg_state) = app.try_state::<ConfigState>() else {
            return;
        };
        let Some(token) = cfg_state.snapshot().sync.token else {
            return;
        };
        let Some((device, raw_id, origin_addr)) = resolve_fetch_target(&state.remote.lock().unwrap(), &session_id) else {
            return;
        };
        emit_history_loading(&app, &session_id, true);
        let url = format!("{origin_addr}/api/sync/dialog");
        let query = [("id", raw_id.clone()), ("since", "0".to_string())];
        let full: Option<Vec<DialogEntry>> = get_json(&url, &query, &token, "dialog catch-up").await;
        if let Some(entries) = full.filter(|e| !e.is_empty()) {
            tracing::debug!(session = %session_id, entries = entries.len(), "dialog catch-up merged");
            let merged = {
                let mut remote = state.remote.lock().unwrap();
                remote.get_mut(&device).and_then(|dev| dev.sessions.iter_mut().find(|s| s.id == session_id)).map(|s| {
                    merge_dialog_entries(&mut s.dialog, &entries);
                    s.clone()
                })
            };
            if let Some(s) = merged {
                if let Some(store) = app.try_state::<RemoteHistoryStore>() {
                    store.save_device(&device, std::slice::from_ref(&s));
                }
                emit_sessions_updated_remote(&app);
            }
        }
        emit_history_loading(&app, &session_id, false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DialogRole, Status};

    fn session(id: &str, dialog: Vec<DialogEntry>) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
            label: "label".into(),
            original_prompt: None,
            task_started_at: 0,
            dialog,
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: None,
            instruction_drift: false,
            canary: crate::state::Canary::Off,
            attended_at: None,
        }
    }

    fn entry(role: DialogRole, text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role, text: text.into(), timestamp: ts, status: Status::Working, task_start: false }
    }
    fn push_item(id: &str, dialog_tip: i64) -> SessionSync {
        SessionSync { session: session(id, Vec::new()), dialog_tip }
    }

    // -------- ingest --------

    /// Shorthand for the common no-persisted-dialogs case.
    fn no_persisted() -> HashMap<String, Vec<DialogEntry>> {
        HashMap::new()
    }

    /// axum applies a layer only to routes registered *before* it, so a
    /// `.route(...)` appended below `.layer(...)` serves with no source check and
    /// no token — and compiles, type-checks and passes every other test here.
    /// Nothing in the type system catches that, and the router needs an
    /// `AppHandle` that a unit test cannot construct, so this asserts the
    /// ordering invariant against the source text itself. Unusual, but it is the
    /// only thing that fails when someone adds the natural next route in the
    /// natural next place — which is exactly what a later stage will do.
    #[test]
    fn guard_covers_every_route() {
        let src = include_str!("sync.rs");
        let start = src.find("let router = Router::new()").expect("router construction moved — update this test");
        let expr = &src[start..];
        let end = expr.find(".with_state(").expect("router must end with .with_state");
        let expr = &expr[..end];

        let layer_at = expr.find(".layer(").expect("the guard layer is gone — every route is now unguarded");
        let last_route_at = expr.rfind(".route(").expect("no routes found");
        assert!(
            last_route_at < layer_at,
            "a route is registered BELOW the guard layer, so it serves with no source check and no bearer token. \
             Move every .route(...) above the .layer(...) call."
        );
        // Guards the guard: if the routes ever stop being registered in this
        // expression the assert above passes vacuously, so pin the count too.
        // 4 -> 5 with `POST /api/sync/message`, the cross-machine message hop.
        // It is the route the comment above predicted, and the one where an
        // unguarded registration would be worst: it starts a turn inside a live
        // agent rather than reading state.
        //
        // 5 -> 6 with `POST /api/sync/grant`, which takes that title from it:
        // the message route acts once, this one writes a standing permission
        // that outlives every message and every restart.
        assert_eq!(expr.matches(".route(").count(), 6, "route count changed — confirm the new route is above the layer, then update this number");
    }

    #[test]
    fn origin_url_brackets_an_ipv6_literal() {
        // Unbracketed, the trailing :port reads as another hextet and the URL
        // fails to parse — silently, since the push itself still answers 204.
        assert_eq!(origin_url("fd7a:115c:a1e0::8735:895b".parse().unwrap(), 9078), "http://[fd7a:115c:a1e0::8735:895b]:9078");
        assert_eq!(origin_url("100.67.137.90".parse().unwrap(), 9078), "http://100.67.137.90:9078");
        // A v4-mapped arrival yields the plain dotted form, not a bracketed literal.
        assert_eq!(origin_url("::ffff:100.67.137.90".parse().unwrap(), 9078), "http://100.67.137.90:9078");
    }

    #[test]
    fn ingest_namespaces_and_stamps_origin() {
        let (dev, _) = ingest("laptop", vec![push_item("proj", 0)], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, "http://1.2.3.4:9078".into());
        assert_eq!(dev.sessions.len(), 1);
        assert_eq!(dev.sessions[0].id, "laptop/proj");
        assert_eq!(dev.sessions[0].origin.as_deref(), Some("laptop"));
        assert_eq!(dev.last_seen, 100);
        assert_eq!(dev.origin_addr, "http://1.2.3.4:9078");
    }

    #[test]
    fn ingest_requests_the_range_above_what_is_held() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "held", 10)]);
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, crate::tailnet::Attestation::Claimed, None, &persisted, 100, String::new());
        assert_eq!(pulls, vec![DialogPull { raw_id: "proj".into(), since: 10 }]);
    }

    #[test]
    fn ingest_requests_everything_when_nothing_is_held() {
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        assert_eq!(pulls, vec![DialogPull { raw_id: "proj".into(), since: 0 }], "since 0 is the complete dialog");
    }

    #[test]
    fn ingest_requests_nothing_when_caught_up() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::Assistant, "a", 70)]);
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, crate::tailnet::Attestation::Claimed, None, &persisted, 100, String::new());
        assert!(pulls.is_empty(), "tip equal to held needs no fetch — this is the steady state");
    }

    #[test]
    fn ingest_carries_held_dialog_across_the_metadata_replace() {
        let (first, _) = ingest("laptop", vec![push_item("proj", 0)], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        let mut seeded = first;
        seeded.sessions[0].dialog = vec![entry(DialogRole::User, "u1", 10)];
        // A later metadata-only push must not wipe the dialog we accumulated.
        let (second, pulls) = ingest("laptop", vec![push_item("proj", 10)], None, crate::tailnet::Attestation::Claimed, Some(&seeded), &no_persisted(), 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 1);
        assert_eq!(second.sessions[0].dialog[0].text, "u1");
        assert!(pulls.is_empty());
    }

    #[test]
    fn ingest_seeds_dialog_from_persisted_when_no_prev() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "old", 10)]);
        let (dev, _) = ingest("laptop", vec![push_item("proj", 10)], None, crate::tailnet::Attestation::Claimed, None, &persisted, 100, String::new());
        assert_eq!(dev.sessions[0].dialog.len(), 1, "disk dialog restored after a restart");
        assert_eq!(dev.sessions[0].dialog[0].text, "old");
    }

    #[test]
    fn ingest_prefers_in_memory_dialog_over_persisted() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "stale-disk", 10)]);
        let (first, _) = ingest("laptop", vec![push_item("proj", 0)], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        let mut seeded = first;
        seeded.sessions[0].dialog = vec![entry(DialogRole::User, "live", 30)];
        let (second, _) = ingest("laptop", vec![push_item("proj", 30)], None, crate::tailnet::Attestation::Claimed, Some(&seeded), &persisted, 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 1, "accumulated in-memory dialog wins");
        assert_eq!(second.sessions[0].dialog[0].text, "live");
    }

    #[test]
    fn ingest_drops_sessions_absent_from_snapshot() {
        let (first, _) = ingest("laptop", vec![push_item("alive", 0), push_item("gone", 0)], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        let (second, _) = ingest("laptop", vec![push_item("alive", 0)], None, crate::tailnet::Attestation::Claimed, Some(&first), &no_persisted(), 200, String::new());
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].id, "laptop/alive");
    }

    #[test]
    fn ingest_clears_display_name_from_sender() {
        let mut item = push_item("proj", 0);
        item.session.display_name = Some("sender name".into());
        let (dev, _) = ingest("laptop", vec![item], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        assert_eq!(dev.sessions[0].display_name, None, "receiver's custom names win");
    }

    // -------- build_push --------

    #[test]
    fn build_push_strips_dialog_to_a_tip() {
        let sessions = vec![session(
            "proj",
            vec![entry(DialogRole::User, "old", 10), entry(DialogRole::User, "new", 100)],
        )];
        let push = build_push("desktop", 9078, &sessions, 4242, 77, None);
        assert_eq!(push.device_name, "desktop");
        assert_eq!(push.listen_port, 9078);
        assert!(push.sessions[0].session.dialog.is_empty(), "no dialog content on the wire");
        assert_eq!(push.sessions[0].dialog_tip, 100, "tip is our newest entry");
        assert_eq!(push.usage_tip, 4242);
        assert_eq!(push.token_tip, 77, "token tip is a seq, advertised alongside the usage timestamp");
    }

    /// Registry rows ride the push so a peer's roster can see sessions the hook
    /// stream never reported. Without this, discovery is narrower than delivery.
    #[test]
    fn build_push_carries_the_registry_and_preserves_no_answer() {
        let regs = vec![crate::session_registry::LiveSession {
            chat_id: "transcripts".into(),
            name: Some("transcripts-87".into()),
            activity: crate::session_registry::Activity::Busy,
            activity_age_ms: Some(1_500),
            sessions: 2,
            session_ids: vec!["abc".into()],
            pid: 4_242,
        }];
        let push = build_push("desktop", 9078, &[], 0, 0, Some(&regs));
        let rows = push.registry_sessions.expect("registry rows ride the push");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id, "transcripts", "un-namespaced; the receiver stamps its own prefix");
        assert_eq!(rows[0].activity, crate::session_registry::Activity::Busy);
        assert_eq!(rows[0].activity_age_ms, Some(1_500));
        assert_eq!(rows[0].sessions, 2);

        // An unreadable registry must stay `None` all the way across, or the
        // peer reads "that machine is running nothing" from our failure to look.
        assert!(build_push("desktop", 9078, &[], 0, 0, None).registry_sessions.is_none());
    }

    #[test]
    fn ingest_stores_the_registry_rows_verbatim() {
        let rows = vec![RegistrySync {
            chat_id: "transcripts".into(),
            name: None,
            activity: crate::session_registry::Activity::Idle,
            activity_age_ms: Some(10),
            sessions: 1,
        }];
        let (dev, _) = ingest("laptop", vec![], Some(rows.clone()), crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        assert_eq!(dev.registry_sessions.as_deref(), Some(&rows[..]));
        let (none, _) = ingest("laptop", vec![], None, crate::tailnet::Attestation::Claimed, None, &no_persisted(), 100, String::new());
        assert!(none.registry_sessions.is_none(), "no answer stays no answer");
    }

    /// A peer old enough not to send the field parses, and lands in the same
    /// "no answer" state as an unreadable registry — which is the truth for the
    /// receiver either way.
    #[test]
    fn a_push_without_registry_sessions_parses_as_no_answer() {
        let body = r#"{"device_name":"old","listen_port":9078,"sessions":[],"usage_tip":5}"#;
        let push: SyncPush = serde_json::from_str(body).expect("older push should parse");
        assert!(push.registry_sessions.is_none());
    }

    /// The registry's own vocabulary is `idle`/`busy`; anything else a future
    /// build sends must land on `Unknown` rather than failing the whole push.
    #[test]
    fn an_unrecognized_activity_parses_as_unknown() {
        let row: RegistrySync = serde_json::from_str(r#"{"chat_id":"p","activity":"compacting","sessions":1}"#).expect("parses");
        assert_eq!(row.activity, crate::session_registry::Activity::Unknown);
    }

    #[test]
    fn a_push_from_an_older_peer_parses_with_no_token_tip() {
        // Rollout skew: a peer that predates token sync sends no `token_tip`.
        // It must still parse and simply contribute nothing, rather than
        // failing the whole push and taking session sync down with it.
        let body = r#"{"device_name":"old","listen_port":9078,"sessions":[],"usage_tip":5}"#;
        let push: SyncPush = serde_json::from_str(body).expect("older push should parse");
        assert_eq!(push.token_tip, 0);
        assert_eq!(push.usage_tip, 5);
    }

    #[test]
    fn build_push_empty_dialog_tips_zero() {
        let sessions = vec![session("proj", Vec::new())];
        assert_eq!(build_push("desktop", 9078, &sessions, 0, 0, None).sessions[0].dialog_tip, 0);
    }

    #[test]
    fn build_push_is_identical_for_every_peer_and_cycle() {
        // The point of going stateless: the push depends only on local data, so
        // there is no per-peer bookkeeping that can go stale, and re-sending is
        // free. A peer that missed ten cycles is caught up by the next one.
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "u", 10)])];
        let a = build_push("desktop", 9078, &sessions, 7, 3, None);
        let b = build_push("desktop", 9078, &sessions, 7, 3, None);
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }

    // -------- usage_since --------

    fn rec(ts: i64) -> UsageHistoryRecord {
        UsageHistoryRecord { ts, five_hour_pct: Some(1.0), seven_day_pct: None, five_hour_resets_at: None, seven_day_resets_at: None }
    }

    #[test]
    fn usage_since_selects_strictly_newer() {
        let records = vec![rec(10), rec(20), rec(30)];
        let delta = usage_since(&records, 20);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].ts, 30);
        assert_eq!(usage_since(&records, 0).len(), 3, "since 0 is the whole timeline");
        assert!(usage_since(&records, 30).is_empty(), "nothing past the newest");
    }

    // -------- resolve_fetch_target --------

    #[test]
    fn resolve_fetch_target_parses_namespaced_id() {
        let mut remote = std::collections::BTreeMap::new();
        let mut s = session("laptop/proj", vec![entry(DialogRole::User, "u", 10), entry(DialogRole::Assistant, "a", 70)]);
        s.origin = Some("laptop".into());
        remote.insert("laptop".to_string(), RemoteDevice { sessions: vec![s], last_seen: 0, origin_addr: "http://1.2.3.4:9078".into(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        let (device, raw_id, addr) = resolve_fetch_target(&remote, "laptop/proj").expect("target");
        assert_eq!(device, "laptop");
        assert_eq!(raw_id, "proj");
        assert_eq!(addr, "http://1.2.3.4:9078");
    }

    /// A device name may contain a slash, so the prefix match must be
    /// longest-first. The `BTreeMap`'s `.find()` used to take `win` here and
    /// hand back a raw id of `box/transcripts`, which exists on no machine —
    /// deterministically wrong, and silent, since a pull for a nonexistent id
    /// simply 404s and is logged as a routine miss.
    #[test]
    fn resolve_fetch_target_prefers_the_longest_device_name() {
        let mut remote = std::collections::BTreeMap::new();
        for d in ["win", "win/box"] {
            remote.insert(d.to_string(), RemoteDevice { sessions: Vec::new(), last_seen: 0, origin_addr: format!("http://{}:9078", d.replace('/', "-")), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        }
        let (device, raw_id, _) = resolve_fetch_target(&remote, "win/box/transcripts").expect("target");
        assert_eq!((device.as_str(), raw_id.as_str()), ("win/box", "transcripts"));
        let (device, raw_id, _) = resolve_fetch_target(&remote, "win/transcripts").expect("target");
        assert_eq!((device.as_str(), raw_id.as_str()), ("win", "transcripts"));
    }

    #[test]
    fn resolve_fetch_target_is_none_for_local_ids() {
        let mut remote = std::collections::BTreeMap::new();
        remote.insert("laptop".to_string(), RemoteDevice { sessions: Vec::new(), last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        assert!(resolve_fetch_target(&remote, "my-local-project").is_none());
        assert!(resolve_fetch_target(&remote, "laptopish/proj").is_none(), "prefix must match a whole device name");
    }

    // -------- the message hop --------

    /// Rollout skew, the same rule `token_tip` follows: an envelope from a peer
    /// that predates a field must parse and degrade, never fail the request.
    /// Failing it would turn a version mismatch into an unexplained refusal on
    /// the one route a user is watching synchronously.
    #[test]
    fn a_message_envelope_from_an_older_peer_parses() {
        let body = r#"{"origin_device":"air","message_id":"air-1-0","target_project":"transcripts","text":"hi"}"#;
        let env: MessageEnvelope = serde_json::from_str(body).expect("older envelope should parse");
        assert_eq!(env.from_agent, "");
        assert_eq!(env.from_label, None);
        assert_eq!(env.text, "hi");
    }

    /// The distinction the whole receipt vocabulary turns on: a refused
    /// connection proves nothing was written, a lost answer proves nothing at
    /// all. Guessing `Unreachable` for a timeout would invite the retry that
    /// writes the message twice.
    #[test]
    fn a_lost_hop_response_is_unknown_and_a_refused_connection_is_unreachable() {
        assert_eq!(hop_failure_outcome(true, false), Outcome::Unreachable);
        assert_eq!(hop_failure_outcome(false, true), Outcome::Unknown);
        assert_eq!(hop_failure_outcome(false, false), Outcome::Unknown, "any other transport failure is also 'answer lost'");
        assert_eq!(hop_failure_outcome(true, true), Outcome::Unknown, "a connect that timed out may still have been established");
    }

    /// `Unknown` must not be a 5xx. A 5xx reads as "it failed, retry", and this
    /// is the one outcome where retrying can produce a second message.
    #[test]
    fn an_unknown_outcome_answers_200_not_an_error_status() {
        let receipt = |o: Outcome| Receipt::new(o, "air-1-0", "chrome/p", Some("chrome"));
        assert_eq!(receipt_status(&receipt(Outcome::Unknown)), StatusCode::OK);
        assert_eq!(receipt_status(&receipt(Outcome::Written)), StatusCode::OK);
        assert_eq!(receipt_status(&receipt(Outcome::Duplicate)), StatusCode::OK);
        assert_eq!(receipt_status(&receipt(Outcome::Unreachable)), StatusCode::BAD_GATEWAY);
        assert_eq!(receipt_status(&receipt(Outcome::Refused)), StatusCode::BAD_REQUEST);
    }

    /// The caller only ever sees this leg, so a refusal the *peer* made has to
    /// keep its own status. Flattening them to 400 would report "your request
    /// was malformed" for "no such session over here" — a different problem with
    /// a different fix.
    #[test]
    fn a_peers_refusal_keeps_its_status_across_the_relay() {
        let refused = |reason: &str| receipt_status(&Receipt::new(Outcome::Refused, "air-1-0", "chrome/p", Some("chrome")).because(reason));
        assert_eq!(refused("no_such_session"), StatusCode::NOT_FOUND);
        assert_eq!(refused("ambiguous_target"), StatusCode::CONFLICT);
        assert_eq!(refused("too_large"), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(refused("registry_unreadable"), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused("local_target"), StatusCode::BAD_REQUEST, "an unmapped reason stays a plain client error");
    }

    /// Every auto-start refusal needs its own arm. Falling through to the `_`
    /// default tells a caller its request was malformed, when what actually
    /// happened is that the target is absent, the machine declined, or it is
    /// mid-start — three different things to do next, none of them "fix your
    /// request".
    #[test]
    fn an_auto_start_refusal_is_not_reported_as_a_bad_request() {
        let refused = |reason: &str| receipt_status(&Receipt::new(Outcome::Refused, "air-1-0", "chrome/p", Some("chrome")).because(reason));
        for reason in ["start_not_listed", "start_path_mismatch", "start_no_directory"] {
            assert_eq!(refused(reason), StatusCode::NOT_FOUND, "{reason} is an absent target");
        }
        assert_eq!(refused("start_unattested"), StatusCode::FORBIDDEN);
        assert_eq!(refused("start_already_running"), StatusCode::CONFLICT);
        for reason in ["start_untrusted_directory", "start_no_launcher", "start_not_realized"] {
            assert_eq!(refused(reason), StatusCode::SERVICE_UNAVAILABLE, "{reason} is the machine declining, not the caller erring");
        }
        assert_eq!(refused("local_target_started"), refused("local_target"), "a local start still refuses the relay, so it answers like the refusal it is");
        assert_ne!(
            receipt_status(&Receipt::new(Outcome::Unreachable, "air-1-0", "chrome/p", Some("chrome")).because("start_not_ready")),
            StatusCode::BAD_REQUEST,
            "a started-but-not-yet-listening session is an Unreachable, judged by outcome rather than by slug"
        );
    }

    /// The auto-start path must be reachable from exactly one registry answer.
    /// `Ambiguous` and `NoInbox` already have a session in that directory and
    /// `Unreadable` means we could not look — starting on any of those is how a
    /// machine ends up with two agents in one project, which makes the user's
    /// own session permanently unmessageable.
    #[test]
    fn only_a_definite_absence_can_trigger_a_start() {
        let starts = |l: &InboxLookup| matches!(l, InboxLookup::NotFound);
        assert!(starts(&InboxLookup::NotFound));
        assert!(!starts(&InboxLookup::Ambiguous { sessions: 2 }));
        assert!(!starts(&InboxLookup::NoInbox));
        assert!(!starts(&InboxLookup::Unreadable));
        assert!(!starts(&InboxLookup::Found { pid: 1, socket_path: "/tmp/s".into() }));
    }

    /// The sending half of the property above. `receipt_status` deliberately
    /// derives the status *from* the receipt, so reading the status first on the
    /// way back undoes it: on 2026-08-31 a real `404 no_such_session` was
    /// reported as `peer_lacks_route`, sending the operator after a version skew
    /// that did not exist while the peer's own body said plainly that nothing
    /// was running under that name. Body first, always.
    #[test]
    fn a_peers_receipt_outranks_the_status_that_carried_it() {
        let peer = |reason: &str| serde_json::to_vec(&Receipt::new(Outcome::Refused, "air-1-0", "chrome/p", Some("chrome")).because(reason)).unwrap();
        let reason_of = |status, body: Vec<u8>| match hop_answer(status, &body) {
            HopAnswer::Peer(r) => r.reason.unwrap_or_default(),
            HopAnswer::Status(_, reason, _) => format!("<status:{reason}>"),
        };
        assert_eq!(reason_of(StatusCode::NOT_FOUND, peer("no_such_session")), "no_such_session");
        assert_eq!(reason_of(StatusCode::CONFLICT, peer("ambiguous_target")), "ambiguous_target");
        assert_eq!(reason_of(StatusCode::SERVICE_UNAVAILABLE, peer("registry_unreadable")), "registry_unreadable");
        assert_eq!(reason_of(StatusCode::PAYLOAD_TOO_LARGE, peer("too_large")), "too_large");
    }

    /// …and the status map still covers every answer the handler never produced:
    /// the guard rejects with a bare status, and a peer predating the route 404s
    /// out of axum's fallback. Both must stay certain refusals rather than
    /// degrading to `unknown`, which would read as "may have been written".
    #[test]
    fn an_answer_with_no_receipt_falls_back_to_the_status() {
        let judge = |status| match hop_answer(status, b"") {
            HopAnswer::Status(outcome, reason, _) => (outcome, reason),
            HopAnswer::Peer(_) => unreachable!("an empty body is not a receipt"),
        };
        assert_eq!(judge(StatusCode::UNAUTHORIZED), (Outcome::Refused, "peer_rejected_token"));
        assert_eq!(judge(StatusCode::FORBIDDEN), (Outcome::Refused, "peer_refused_source"));
        assert_eq!(judge(StatusCode::NOT_FOUND), (Outcome::Refused, "peer_lacks_route"), "no body: a bare 404 really is a peer without the route");
        assert_eq!(judge(StatusCode::METHOD_NOT_ALLOWED), (Outcome::Refused, "peer_lacks_route"));
        assert_eq!(judge(StatusCode::BAD_GATEWAY), (Outcome::Unreachable, "peer_error"));
        assert_eq!(judge(StatusCode::OK), (Outcome::Unknown, "unreadable_receipt"), "a success we cannot read is the one honest 'we do not know'");
    }

    /// The message route sits behind the same two gates as every other one —
    /// asserted structurally by `guard_covers_every_route` (it is above the
    /// layer) and behaviourally by the `source_allowed` / `bearer_ok` cases
    /// below, which are the gate's whole logic. Restated here because this is
    /// the first route that starts a turn rather than serving a read.
    #[test]
    fn the_message_route_is_refused_off_tailnet_and_without_a_bearer() {
        assert!(!source_allowed(TAILNET, ip("192.168.1.5")), "an off-tailnet source never reaches the token compare");
        assert!(!bearer_ok(&HeaderMap::new(), Some("s3cret")), "no credential, no relay");
        assert!(!bearer_ok(&headers_with("Bearer nope"), Some("s3cret")));
        assert!(bearer_ok(&headers_with("Bearer s3cret"), Some("s3cret")));
    }

    // -------- bearer_ok --------

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", value.parse().unwrap());
        h
    }

    #[test]
    fn bearer_accepts_matching_token() {
        assert!(bearer_ok(&headers_with("Bearer s3cret"), Some("s3cret")));
    }

    #[test]
    fn bearer_rejects_wrong_missing_or_unconfigured() {
        assert!(!bearer_ok(&headers_with("Bearer nope"), Some("s3cret")));
        assert!(!bearer_ok(&HeaderMap::new(), Some("s3cret")));
        assert!(!bearer_ok(&headers_with("Bearer s3cret"), None), "no token = sync disabled");
        assert!(!bearer_ok(&headers_with("Bearer "), Some("")), "empty token never matches");
    }

    #[test]
    fn bearer_rejects_a_prefix_or_a_trailing_byte() {
        // Both length directions, because length is folded into the comparison
        // rather than short-circuiting ahead of it — a wrong length must be as
        // wrong as a wrong byte, and no cheaper to discover.
        assert!(!bearer_ok(&headers_with("Bearer s3c"), Some("s3cret")));
        assert!(!bearer_ok(&headers_with("Bearer s3crets"), Some("s3cret")));
    }

    #[test]
    fn bearer_rejects_a_non_bearer_scheme() {
        assert!(!bearer_ok(&headers_with("Basic s3cret"), Some("s3cret")));
        assert!(!bearer_ok(&headers_with("s3cret"), Some("s3cret")), "credentials without the scheme");
    }

    #[test]
    fn tokens_match_only_on_identical_bytes() {
        assert!(tokens_match(b"s3cret", b"s3cret"));
        assert!(!tokens_match(b"S3cret", b"s3cret"), "first byte, case-sensitive");
        assert!(!tokens_match(b"s3creT", b"s3cret"), "last byte");
        assert!(!tokens_match(b"s3cre", b"s3cret"), "a prefix of the secret");
        assert!(!tokens_match(b"s3cretX", b"s3cret"), "the secret plus a suffix");
        assert!(!tokens_match(b"", b"s3cret"));
        // The helper is pure byte equality; "an empty configured token never
        // matches" is `bearer_ok`'s rule and lives in exactly one place.
        assert!(tokens_match(b"", b""));
    }

    // -------- source_allowed --------

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address")
    }
    const TAILNET: SyncBindScope = SyncBindScope::Tailnet;

    #[test]
    fn source_allows_tailnet_v4() {
        for a in ["100.64.0.1", "100.86.97.31", "100.67.137.90", "100.127.255.254"] {
            assert!(source_allowed(TAILNET, ip(a)), "{a} is inside 100.64.0.0/10");
        }
    }

    #[test]
    fn source_rejects_addresses_just_outside_the_cgnat_range() {
        // The mask is /10, not a "100." string prefix and not a /8: both of
        // these are ordinary public addresses.
        assert!(!source_allowed(TAILNET, ip("100.63.255.255")));
        assert!(!source_allowed(TAILNET, ip("100.128.0.0")));
    }

    #[test]
    fn source_allows_tailnet_v6_and_rejects_other_private_v6() {
        assert!(source_allowed(TAILNET, ip("fd7a:115c:a1e0::1")), "Tailscale's ULA prefix");
        assert!(source_allowed(TAILNET, ip("fd7a:115c:a1e0::8735:895b")));
        assert!(!source_allowed(TAILNET, ip("fd00::1")), "matched on the /48, not on 'any ULA'");
        assert!(!source_allowed(TAILNET, ip("fe80::1")), "link-local is not the tailnet");
    }

    #[test]
    fn source_allows_loopback_v4_and_v6() {
        // The documented localhost observer-peer harness pushes from here.
        assert!(source_allowed(TAILNET, ip("127.0.0.1")));
        assert!(source_allowed(TAILNET, ip("127.0.0.2")));
        assert!(source_allowed(TAILNET, ip("::1")));
    }

    #[test]
    fn source_allows_v4_mapped_addresses() {
        // A dual-stack socket reports v4 peers mapped; classify after unmapping
        // or every tailnet peer reads as an unknown v6 address.
        assert!(source_allowed(TAILNET, ip("::ffff:100.86.97.31")));
        assert!(source_allowed(TAILNET, ip("::ffff:127.0.0.1")));
        assert!(!source_allowed(TAILNET, ip("::ffff:192.168.1.5")));
    }

    #[test]
    fn source_rejects_lan_and_public_addresses() {
        for a in ["192.168.1.5", "10.0.0.5", "172.16.0.9", "1.2.3.4"] {
            assert!(!source_allowed(TAILNET, ip(a)), "{a}: a home router is not the boundary");
        }
    }

    #[test]
    fn source_check_allows_everything_under_the_any_scope() {
        for a in ["1.2.3.4", "192.168.1.5", "fe80::1", "127.0.0.1"] {
            assert!(source_allowed(SyncBindScope::Any, ip(a)), "`any` is the escape hatch, in both directions");
        }
    }

    // -------- select_binds --------

    #[test]
    fn bind_prefers_the_tailnet_addresses_and_keeps_loopback() {
        let plan = select_binds(TAILNET, &[ip("100.67.137.90"), ip("fd7a:115c:a1e0::1")], 9078);
        assert!(!plan.degraded);
        assert_eq!(
            plan.addrs,
            vec![
                SocketAddr::new(ip("100.67.137.90"), 9078),
                SocketAddr::new(ip("fd7a:115c:a1e0::1"), 9078),
                SocketAddr::new(ip("127.0.0.1"), 9078),
                SocketAddr::new(ip("::1"), 9078),
            ],
            "a narrow bind serves no loopback of its own — the observer-peer harness needs one"
        );
    }

    #[test]
    fn bind_falls_back_to_all_interfaces_when_no_tailnet_address() {
        let plan = select_binds(TAILNET, &[], 9078);
        assert_eq!(plan.addrs, vec![SocketAddr::new(ip("0.0.0.0"), 9078)]);
        assert!(plan.degraded, "the flag is what makes the wide bind observable — sync must not simply fail to start");
    }

    #[test]
    fn bind_ignores_lan_and_link_local_candidates() {
        // The route lookup answers with the LAN address when Tailscale is down;
        // binding that would serve the LAN and not the tailnet.
        let plan = select_binds(TAILNET, &[ip("192.168.1.97"), ip("fe80::1")], 9078);
        assert_eq!(plan.addrs, vec![SocketAddr::new(ip("0.0.0.0"), 9078)]);
        assert!(plan.degraded);
    }

    #[test]
    fn bind_any_scope_is_the_wildcard_and_not_degraded() {
        let plan = select_binds(SyncBindScope::Any, &[ip("100.67.137.90")], 9078);
        assert_eq!(plan.addrs, vec![SocketAddr::new(ip("0.0.0.0"), 9078)]);
        assert!(!plan.degraded, "a wide bind that was asked for is not a degrade");
    }

    // -------- reject_log_due --------

    #[test]
    fn reject_log_is_throttled_per_address() {
        let mut seen = HashMap::new();
        let a = ip("1.2.3.4");
        let b = ip("5.6.7.8");
        assert!(reject_log_due(&mut seen, a, 0), "first sighting always logs");
        assert!(!reject_log_due(&mut seen, a, 1_000), "a retrying peer must not flood the log");
        assert!(reject_log_due(&mut seen, b, 1_000), "throttled per address, not globally");
        assert!(reject_log_due(&mut seen, a, REJECT_LOG_COOLDOWN_MS), "still refused a minute later — say so once more");
    }

    #[test]
    fn reject_log_memory_is_bounded() {
        let mut seen = HashMap::new();
        for i in 0..REJECT_LOG_CAP + 10 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(i as u32));
            reject_log_due(&mut seen, ip, 0);
        }
        assert!(seen.len() <= REJECT_LOG_CAP, "a source-sweeping scan must not grow the map without bound");
    }
}
