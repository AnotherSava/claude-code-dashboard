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
//! The listener binds all interfaces (only the tailnet routes here in
//! practice) and every route requires the shared bearer token; with no token
//! configured sync is fully disabled — never run unauthenticated.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;

use crate::commands::{emit_sessions_updated_remote, emit_usage_limits_updated, now_ms};
use crate::config::ConfigState;
use crate::remote_history::RemoteHistoryStore;
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

/// One dialog range the receiver decided it is missing: `since` is its newest
/// held timestamp, so the origin returns strictly newer entries.
#[derive(Debug, PartialEq)]
struct DialogPull {
    raw_id: String,
    since: i64,
}

/// True when the request carries `Authorization: Bearer <token>` matching the
/// configured shared secret. No configured token = reject everything.
fn bearer_ok(headers: &HeaderMap, token: Option<&str>) -> bool {
    let Some(expected) = token else {
        return false;
    };
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| !expected.is_empty() && t == expected)
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
    (RemoteDevice { sessions: out, last_seen: now, origin_addr }, pulls)
}

async fn post_sync(
    State(app): State<AppHandle>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(push): Json<SyncPush>,
) -> Result<StatusCode, StatusCode> {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let cfg = cfg_state.snapshot();
    if !bearer_ok(&headers, cfg.sync.token.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let Some(state) = app.try_state::<AppState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if push.device_name.is_empty() || push.device_name == cfg.sync.device_name {
        tracing::warn!(device = %push.device_name, "sync push rejected: empty or same device_name as ours");
        return Err(StatusCode::BAD_REQUEST);
    }
    tracing::debug!(
        device = %push.device_name,
        sessions = push.sessions.len(),
        "sync push received"
    );
    let origin_addr = format!("http://{}:{}", addr.ip(), push.listen_port);
    let now = now_ms();
    let store = app.try_state::<RemoteHistoryStore>();
    let persisted = store.as_ref().map(|s| s.device_dialogs(&push.device_name)).unwrap_or_default();
    let usage_tip = push.usage_tip;
    let token_tip = push.token_tip;
    let device_name = push.device_name.clone();
    let pulls = {
        let mut remote = state.remote.lock().unwrap();
        let prev = remote.get(&push.device_name);
        let (device, pulls) = ingest(&push.device_name, push.sessions, prev, &persisted, now, origin_addr.clone());
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
async fn get_dialog(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Query(q): Query<DialogQuery>,
) -> Result<Json<Vec<DialogEntry>>, StatusCode> {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if !bearer_ok(&headers, cfg_state.snapshot().sync.token.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
async fn get_usage(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Vec<UsageHistoryRecord>>, StatusCode> {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if !bearer_ok(&headers, cfg_state.snapshot().sync.token.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
async fn get_tokens(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Result<Json<Vec<crate::token_history::TokenRecord>>, StatusCode> {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    if !bearer_ok(&headers, cfg_state.snapshot().sync.token.as_deref()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let records = app
        .try_state::<crate::token_history::TokenHistoryStore>()
        .map(|s| s.records_since_seq(q.since, MAX_TOKEN_RANGE))
        .unwrap_or_default();
    Ok(Json(records))
}

/// Sync listener on all interfaces — see module docs for why that's safe.
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

pub async fn run_listener(app: AppHandle, port: u16) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "sync bind failed");
            SyncListening::set(&app, false);
            return;
        }
    };
    tracing::info!(%addr, "sync listening");
    SyncListening::set(&app, true);
    let app_for_flag = app.clone();

    let router = Router::new()
        .route("/api/sync", post(post_sync))
        .route("/api/sync/dialog", get(get_dialog))
        .route("/api/sync/usage", get(get_usage))
        .route("/api/sync/tokens", get(get_tokens))
        .with_state(app);

    if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        tracing::error!(error = %e, "sync serve ended");
    }
    SyncListening::set(&app_for_flag, false);
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
fn build_push(device_name: &str, listen_port: u16, sessions: &[AgentSession], usage_tip: i64, token_tip: u64) -> SyncPush {
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
    let push = build_push(&cfg.sync.device_name, cfg.sync.listen_port, &sessions, usage_tip, token_tip);
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
        }
    });
}

/// Resolve a (possibly remote) session id into a catch-up fetch target:
/// the owning device, the raw id on the origin (prefix stripped), and the
/// origin address. `None` for local ids: no remote device prefix matches.
fn resolve_fetch_target(
    remote: &std::collections::BTreeMap<String, RemoteDevice>,
    session_id: &str,
) -> Option<(String, String, String)> {
    remote
        .iter()
        .find(|(d, _)| session_id.starts_with(&format!("{d}/")))
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

    #[test]
    fn ingest_namespaces_and_stamps_origin() {
        let (dev, _) = ingest("laptop", vec![push_item("proj", 0)], None, &no_persisted(), 100, "http://1.2.3.4:9078".into());
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
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, &persisted, 100, String::new());
        assert_eq!(pulls, vec![DialogPull { raw_id: "proj".into(), since: 10 }]);
    }

    #[test]
    fn ingest_requests_everything_when_nothing_is_held() {
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, &no_persisted(), 100, String::new());
        assert_eq!(pulls, vec![DialogPull { raw_id: "proj".into(), since: 0 }], "since 0 is the complete dialog");
    }

    #[test]
    fn ingest_requests_nothing_when_caught_up() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::Assistant, "a", 70)]);
        let (_dev, pulls) = ingest("laptop", vec![push_item("proj", 70)], None, &persisted, 100, String::new());
        assert!(pulls.is_empty(), "tip equal to held needs no fetch — this is the steady state");
    }

    #[test]
    fn ingest_carries_held_dialog_across_the_metadata_replace() {
        let (first, _) = ingest("laptop", vec![push_item("proj", 0)], None, &no_persisted(), 100, String::new());
        let mut seeded = first;
        seeded.sessions[0].dialog = vec![entry(DialogRole::User, "u1", 10)];
        // A later metadata-only push must not wipe the dialog we accumulated.
        let (second, pulls) = ingest("laptop", vec![push_item("proj", 10)], Some(&seeded), &no_persisted(), 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 1);
        assert_eq!(second.sessions[0].dialog[0].text, "u1");
        assert!(pulls.is_empty());
    }

    #[test]
    fn ingest_seeds_dialog_from_persisted_when_no_prev() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "old", 10)]);
        let (dev, _) = ingest("laptop", vec![push_item("proj", 10)], None, &persisted, 100, String::new());
        assert_eq!(dev.sessions[0].dialog.len(), 1, "disk dialog restored after a restart");
        assert_eq!(dev.sessions[0].dialog[0].text, "old");
    }

    #[test]
    fn ingest_prefers_in_memory_dialog_over_persisted() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "stale-disk", 10)]);
        let (first, _) = ingest("laptop", vec![push_item("proj", 0)], None, &no_persisted(), 100, String::new());
        let mut seeded = first;
        seeded.sessions[0].dialog = vec![entry(DialogRole::User, "live", 30)];
        let (second, _) = ingest("laptop", vec![push_item("proj", 30)], Some(&seeded), &persisted, 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 1, "accumulated in-memory dialog wins");
        assert_eq!(second.sessions[0].dialog[0].text, "live");
    }

    #[test]
    fn ingest_drops_sessions_absent_from_snapshot() {
        let (first, _) = ingest("laptop", vec![push_item("alive", 0), push_item("gone", 0)], None, &no_persisted(), 100, String::new());
        let (second, _) = ingest("laptop", vec![push_item("alive", 0)], Some(&first), &no_persisted(), 200, String::new());
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].id, "laptop/alive");
    }

    #[test]
    fn ingest_clears_display_name_from_sender() {
        let mut item = push_item("proj", 0);
        item.session.display_name = Some("sender name".into());
        let (dev, _) = ingest("laptop", vec![item], None, &no_persisted(), 100, String::new());
        assert_eq!(dev.sessions[0].display_name, None, "receiver's custom names win");
    }

    // -------- build_push --------

    #[test]
    fn build_push_strips_dialog_to_a_tip() {
        let sessions = vec![session(
            "proj",
            vec![entry(DialogRole::User, "old", 10), entry(DialogRole::User, "new", 100)],
        )];
        let push = build_push("desktop", 9078, &sessions, 4242, 77);
        assert_eq!(push.device_name, "desktop");
        assert_eq!(push.listen_port, 9078);
        assert!(push.sessions[0].session.dialog.is_empty(), "no dialog content on the wire");
        assert_eq!(push.sessions[0].dialog_tip, 100, "tip is our newest entry");
        assert_eq!(push.usage_tip, 4242);
        assert_eq!(push.token_tip, 77, "token tip is a seq, advertised alongside the usage timestamp");
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
        assert_eq!(build_push("desktop", 9078, &sessions, 0, 0).sessions[0].dialog_tip, 0);
    }

    #[test]
    fn build_push_is_identical_for_every_peer_and_cycle() {
        // The point of going stateless: the push depends only on local data, so
        // there is no per-peer bookkeeping that can go stale, and re-sending is
        // free. A peer that missed ten cycles is caught up by the next one.
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "u", 10)])];
        let a = build_push("desktop", 9078, &sessions, 7, 3);
        let b = build_push("desktop", 9078, &sessions, 7, 3);
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
        remote.insert("laptop".to_string(), RemoteDevice { sessions: vec![s], last_seen: 0, origin_addr: "http://1.2.3.4:9078".into() });
        let (device, raw_id, addr) = resolve_fetch_target(&remote, "laptop/proj").expect("target");
        assert_eq!(device, "laptop");
        assert_eq!(raw_id, "proj");
        assert_eq!(addr, "http://1.2.3.4:9078");
    }

    #[test]
    fn resolve_fetch_target_is_none_for_local_ids() {
        let mut remote = std::collections::BTreeMap::new();
        remote.insert("laptop".to_string(), RemoteDevice { sessions: Vec::new(), last_seen: 0, origin_addr: String::new() });
        assert!(resolve_fetch_target(&remote, "my-local-project").is_none());
        assert!(resolve_fetch_target(&remote, "laptopish/proj").is_none(), "prefix must match a whole device name");
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
}
