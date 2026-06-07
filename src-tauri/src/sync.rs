//! Multi-device session sync. Every device runs the dashboard; each pushes
//! its *local* sessions to the peers in `config.sync.peers` and renders the
//! remote sessions it receives (stored in `AppState::remote`, merged into the
//! frontend payload by `commands::resolved_snapshot`).
//!
//! Single-writer model: each device is authoritative for its own sessions.
//! A push carries a full snapshot of the sender's session metadata (receiver
//! wholesale-replaces it per device; removal = absence) plus per-session
//! dialog *deltas* — entries changed since the peer's watermark — which the
//! receiver accumulates via `state::merge_dialog_entries`. Dialogs run to
//! hundreds of KB, so resending them on every push would be wasteful, while
//! metadata-only pushes would leave an open history window stale.
//!
//! The listener binds all interfaces (only the tailnet routes here in
//! practice) and every route requires the shared bearer token; with no token
//! configured sync is fully disabled — never run unauthenticated.

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

use crate::commands::{emit_sessions_updated_remote, now_ms};
use crate::config::ConfigState;
use crate::remote_history::RemoteHistoryStore;
use crate::state::{merge_dialog_entries, AgentSession, AppState, DialogEntry, RemoteDevice};

/// Coalesce window after a state change before pushing.
const DEBOUNCE_MS: u64 = 300;
/// Periodic push even without local changes — keeps peers' `last_seen` fresh.
const HEARTBEAT_SECS: u64 = 30;
/// Drop a remote device after this long without a push (3 missed heartbeats).
const REMOTE_TTL_MS: i64 = 90_000;
/// Watermark overlap: a peer's watermark advances to the push capture time
/// minus this margin, so entries stamped in the same instant a push was being
/// built can't fall between two pushes. Re-sent entries are deduplicated by
/// `merge_dialog_entries`, so the overlap only costs a few bytes.
const WATERMARK_MARGIN_MS: i64 = 2_000;

/// Poked by `commands::emit_sessions_updated` on every state transition; the
/// pusher debounces and ships local sessions to all peers.
pub struct SyncDirty(pub Arc<Notify>);

/// Wire shape for `POST /api/sync`.
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncPush {
    pub device_name: String,
    /// The sender's own sync listener port; combined with the socket peer IP
    /// it gives the receiver the origin address for catch-up dialog fetches.
    pub listen_port: u16,
    /// The watermark the dialog deltas were selected against (entries with
    /// `timestamp > delta_from` made it in). `0` means the deltas are each
    /// session's complete dialog — a freshly started pusher. Lets the
    /// receiver verify contiguity with what it holds and discard floating
    /// fragments instead of storing dialogs with an invisible gap; `0` is
    /// also the serde default, so a legacy sender without the field keeps
    /// its pushes accepted (the pre-guard behavior).
    #[serde(default)]
    pub delta_from: i64,
    pub sessions: Vec<SessionSync>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionSync {
    /// Full metadata snapshot with `dialog` stripped (it travels as a delta),
    /// raw local id, `origin`/`display_name` unset — the receiver namespaces
    /// and stamps.
    pub session: AgentSession,
    /// Dialog entries changed since this peer's watermark.
    pub dialog_delta: Vec<DialogEntry>,
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
/// namespace ids to "{device}/{raw_id}", stamp `origin`, carry over the
/// dialog accumulated from earlier pushes (sessions absent from the snapshot
/// drop out by not being carried), and merge each delta. Sessions without an
/// in-memory predecessor seed their dialog from `persisted` — the on-disk
/// copy a dashboard restart would otherwise have discarded.
///
/// Contiguity guard: a delta is merged only when `delta_from == 0` (the
/// delta is the complete dialog) or `delta_from <=` the newest held entry
/// (it overlaps what we hold). Anything else is a floating fragment — newer
/// entries with a hole below them, e.g. a fresh install receiving deltas
/// from a long-running origin — and is discarded: held dialogs stay gap-free
/// by construction, and the history window's full-dialog catch-up fills in
/// the rest at the only moment remote dialog is actually read.
fn ingest(
    device: &str,
    sessions: Vec<SessionSync>,
    prev: Option<&RemoteDevice>,
    persisted: &HashMap<String, Vec<DialogEntry>>,
    delta_from: i64,
    now: i64,
    origin_addr: String,
) -> RemoteDevice {
    let mut out = Vec::with_capacity(sessions.len());
    for item in sessions {
        let mut s = item.session;
        s.id = format!("{device}/{}", s.id);
        s.origin = Some(device.to_string());
        s.display_name = None; // receiver's custom names win at emit time
        let mut dialog = prev
            .and_then(|p| p.sessions.iter().find(|ps| ps.id == s.id))
            .map(|ps| ps.dialog.clone())
            .or_else(|| persisted.get(&s.id).cloned())
            .unwrap_or_default();
        let held_max = dialog.iter().map(|e| e.timestamp).max();
        if delta_from == 0 || held_max.is_some_and(|m| delta_from <= m) {
            merge_dialog_entries(&mut dialog, &item.dialog_delta);
        } else if !item.dialog_delta.is_empty() {
            tracing::debug!(session = %s.id, delta_from, held_max, "non-contiguous dialog delta discarded");
        }
        s.dialog = dialog;
        out.push(s);
    }
    RemoteDevice { sessions: out, last_seen: now, origin_addr }
}

async fn post_sync(
    State(app): State<AppHandle>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(push): Json<SyncPush>,
) -> StatusCode {
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let cfg = cfg_state.snapshot();
    if !bearer_ok(&headers, cfg.sync.token.as_deref()) {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    if push.device_name.is_empty() || push.device_name == cfg.sync.device_name {
        tracing::warn!(device = %push.device_name, "sync push rejected: empty or same device_name as ours");
        return StatusCode::BAD_REQUEST;
    }
    let delta_entries = push.sessions.iter().map(|s| s.dialog_delta.len()).sum::<usize>();
    tracing::debug!(
        device = %push.device_name,
        sessions = push.sessions.len(),
        delta_entries,
        "sync push received"
    );
    let origin_addr = format!("http://{}:{}", addr.ip(), push.listen_port);
    let now = now_ms();
    let store = app.try_state::<RemoteHistoryStore>();
    let persisted = store.as_ref().map(|s| s.device_dialogs(&push.device_name)).unwrap_or_default();
    let sessions = {
        let mut remote = state.remote.lock().unwrap();
        let prev = remote.get(&push.device_name);
        let device = ingest(&push.device_name, push.sessions, prev, &persisted, push.delta_from, now, origin_addr);
        let sessions = device.sessions.clone();
        remote.insert(push.device_name.clone(), device);
        sessions
    };
    // Deltas are the only new content — a delta-free push (heartbeat, or one
    // that merely restored dialogs *from* the store) changes nothing on disk.
    if delta_entries > 0 {
        if let Some(store) = store {
            store.save_device(&push.device_name, &sessions);
        }
    }
    emit_sessions_updated_remote(&app);
    StatusCode::NO_CONTENT
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

/// Sync listener on all interfaces — see module docs for why that's safe.
pub async fn run_listener(app: AppHandle, port: u16) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "sync bind failed");
            return;
        }
    };
    tracing::info!(%addr, "sync listening");

    let router = Router::new()
        .route("/api/sync", post(post_sync))
        .route("/api/sync/dialog", get(get_dialog))
        .with_state(app);

    if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        tracing::error!(error = %e, "sync serve ended");
    }
}

/// Per-push dialog-delta budget, in summed entry-text bytes. Bounds every
/// POST: a peer that's far behind gets its backlog drained in successive
/// bounded chunks within one cycle, and a *down* peer costs each failed
/// cycle one bounded build + connect attempt instead of an ever-growing
/// serialization of everything since its frozen watermark.
const DELTA_BUDGET_BYTES: usize = 256 * 1024;

/// One bounded chunk of a peer push.
struct PushChunk {
    push: SyncPush,
    /// Watermark to adopt once this chunk is acknowledged: the last included
    /// timestamp, or `capture - WATERMARK_MARGIN_MS` when drained.
    ack_watermark: i64,
    /// False when the budget cut the backlog — more chunks remain.
    drained: bool,
}

/// Build one push chunk: full metadata for every local session, plus the
/// *oldest* `budget` bytes of dialog backlog above `watermark` (timestamp
/// order across sessions). Oldest-first because the receiver only merges
/// deltas contiguous with what it holds, so backfill must grow upward from
/// the watermark — a newest-first fragment would be discarded. Entries
/// sharing the cut timestamp are never split across chunks (selection is
/// strictly `> watermark`, so a split twin would be skipped forever), and
/// the first entry always fits regardless of size, guaranteeing progress.
fn build_push_chunk(device_name: &str, listen_port: u16, sessions: &[AgentSession], watermark: i64, capture: i64, budget: usize) -> PushChunk {
    let mut pending: Vec<(i64, usize)> = sessions
        .iter()
        .flat_map(|s| s.dialog.iter().filter(|e| e.timestamp > watermark))
        .map(|e| (e.timestamp, e.text.len()))
        .collect();
    pending.sort_unstable_by_key(|&(ts, _)| ts);
    let mut used = 0usize;
    let mut last_included = watermark;
    let mut drained = true;
    for &(ts, len) in &pending {
        if used > 0 && ts > last_included && used + len > budget {
            drained = false;
            break;
        }
        used += len;
        last_included = ts;
    }
    let cutoff = if drained { i64::MAX } else { last_included };
    let push = SyncPush {
        device_name: device_name.to_string(),
        listen_port,
        delta_from: watermark,
        sessions: sessions
            .iter()
            .map(|s| {
                let mut meta = s.clone();
                let dialog_delta = meta.dialog.iter().filter(|e| e.timestamp > watermark && e.timestamp <= cutoff).cloned().collect();
                meta.dialog = Vec::new();
                SessionSync { session: meta, dialog_delta }
            })
            .collect(),
    };
    PushChunk { push, ack_watermark: if drained { capture - WATERMARK_MARGIN_MS } else { last_included }, drained }
}

async fn push_all(app: &AppHandle, client: &reqwest::Client, watermarks: &mut HashMap<String, i64>) {
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
    // Capture time *before* the snapshot: any entry stamped later either made
    // it into this snapshot (then the merge dedups the re-send) or pokes the
    // dirty signal for the next push. Local sessions only — received remote
    // sessions are never re-broadcast.
    let capture = now_ms();
    let sessions = state.snapshot();
    // Cycle breadcrumb: push cadence should never silently stop while peers
    // are configured — if the failure logs go quiet, this shows whether the
    // pusher loop itself is still alive.
    tracing::trace!(peers = cfg.sync.peers.len(), sessions = sessions.len(), "sync push cycle");
    for peer in &cfg.sync.peers {
        let url = format!("{}/api/sync", peer.trim_end_matches('/'));
        // Drain loop: each acknowledged chunk advances the watermark and the
        // next chunk follows immediately, so a peer that was offline catches
        // up within one cycle — but the full backlog is only ever built and
        // sent once the peer has proven reachable, one bounded POST at a time.
        loop {
            let watermark = watermarks.get(peer).copied().unwrap_or(0);
            let chunk = build_push_chunk(&cfg.sync.device_name, cfg.sync.listen_port, &sessions, watermark, capture, DELTA_BUDGET_BYTES);
            match client.post(&url).bearer_auth(&token).json(&chunk.push).send().await {
                Ok(resp) if resp.status().is_success() => {
                    watermarks.insert(peer.clone(), chunk.ack_watermark);
                    if chunk.drained {
                        break;
                    }
                }
                // Failures leave the watermark in place, so the next successful
                // push carries everything the peer missed. Offline peers are
                // routine — log at debug, not warn.
                Ok(resp) => {
                    tracing::debug!(peer = %peer, status = %resp.status(), "sync push rejected");
                    break;
                }
                Err(e) => {
                    tracing::debug!(peer = %peer, error = %e, "sync push failed");
                    break;
                }
            }
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
        let mut watermarks: HashMap<String, i64> = HashMap::new();
        loop {
            tokio::select! {
                _ = dirty.notified() => {
                    tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
                }
                _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)) => {}
            }
            push_all(&app, &client, &mut watermarks).await;
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
async fn fetch_full_dialog(origin_addr: &str, raw_id: &str, token: &str) -> Option<Vec<DialogEntry>> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("reqwest client");
    let url = format!("{origin_addr}/api/sync/dialog");
    match client.get(&url).query(&[("id", raw_id)]).bearer_auth(token).send().await {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(entries) => Some(entries),
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "dialog catch-up parse failed");
                None
            }
        },
        Ok(r) => {
            tracing::debug!(url = %url, status = %r.status(), "dialog catch-up rejected");
            None
        }
        Err(e) => {
            tracing::debug!(url = %url, error = %e, "dialog catch-up failed");
            None
        }
    }
}

/// Catch-up fetch for one remote session's dialog, triggered when the history
/// window targets it. Always fetches the origin's full dialog: what we hold
/// may have a gap *below* its newest entry (a dashboard restart discards the
/// accumulated copy, and fresh push deltas arrive within seconds), so no
/// held timestamp can serve as a "since" watermark — the turn-aware merge
/// dedups the overlap instead. Brackets the fetch in `history_loading`
/// events so the window can show a hint. Fire-and-forget: on failure (origin
/// offline) the window simply shows whatever is held.
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
        if let Some(entries) = fetch_full_dialog(&origin_addr, &raw_id, &token).await.filter(|e| !e.is_empty()) {
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
            display_name: None,
            origin: None,
        }
    }

    fn entry(role: DialogRole, text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role, text: text.into(), timestamp: ts, status: Status::Working, task_start: false }
    }

    fn push_item(id: &str, delta: Vec<DialogEntry>) -> SessionSync {
        SessionSync { session: session(id, Vec::new()), dialog_delta: delta }
    }

    // -------- ingest --------

    /// Shorthand for the common no-persisted-dialogs case.
    fn no_persisted() -> HashMap<String, Vec<DialogEntry>> {
        HashMap::new()
    }

    #[test]
    fn ingest_namespaces_and_stamps_origin() {
        let dev = ingest("laptop", vec![push_item("proj", Vec::new())], None, &no_persisted(), 0, 100, "http://1.2.3.4:9078".into());
        assert_eq!(dev.sessions.len(), 1);
        assert_eq!(dev.sessions[0].id, "laptop/proj");
        assert_eq!(dev.sessions[0].origin.as_deref(), Some("laptop"));
        assert_eq!(dev.last_seen, 100);
        assert_eq!(dev.origin_addr, "http://1.2.3.4:9078");
    }

    #[test]
    fn ingest_accumulates_dialog_across_pushes() {
        let first = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::User, "u1", 10)])], None, &no_persisted(), 0, 100, String::new());
        let second = ingest(
            "laptop",
            vec![push_item("proj", vec![entry(DialogRole::Assistant, "a1", 20)])],
            Some(&first),
            &no_persisted(),
            10,
            200,
            String::new(),
        );
        let dialog = &second.sessions[0].dialog;
        assert_eq!(dialog.len(), 2, "earlier entries survive a delta-only push");
        assert_eq!(dialog[0].text, "u1");
        assert_eq!(dialog[1].text, "a1");
    }

    #[test]
    fn ingest_drops_sessions_absent_from_snapshot() {
        let first = ingest(
            "laptop",
            vec![push_item("alive", Vec::new()), push_item("gone", Vec::new())],
            None,
            &no_persisted(),
            0,
            100,
            String::new(),
        );
        let second = ingest("laptop", vec![push_item("alive", Vec::new())], Some(&first), &no_persisted(), 0, 200, String::new());
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].id, "laptop/alive");
    }

    #[test]
    fn ingest_replayed_delta_does_not_duplicate() {
        let delta = vec![entry(DialogRole::User, "u1", 10), entry(DialogRole::Assistant, "a1", 20)];
        let first = ingest("laptop", vec![push_item("proj", delta.clone())], None, &no_persisted(), 0, 100, String::new());
        let second = ingest("laptop", vec![push_item("proj", delta)], Some(&first), &no_persisted(), 10, 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 2);
    }

    #[test]
    fn ingest_seeds_dialog_from_persisted_when_no_prev() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "old", 10)]);
        let dev = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::Assistant, "new", 20)])], None, &persisted, 10, 100, String::new());
        let dialog = &dev.sessions[0].dialog;
        assert_eq!(dialog.len(), 2, "disk dialog restored under the delta");
        assert_eq!(dialog[0].text, "old");
        assert_eq!(dialog[1].text, "new");
    }

    #[test]
    fn ingest_prefers_in_memory_dialog_over_persisted() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "stale-disk", 10)]);
        let first = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::User, "live", 30)])], None, &no_persisted(), 0, 100, String::new());
        let second = ingest("laptop", vec![push_item("proj", Vec::new())], Some(&first), &persisted, 30, 200, String::new());
        let dialog = &second.sessions[0].dialog;
        assert_eq!(dialog.len(), 1, "accumulated in-memory dialog wins");
        assert_eq!(dialog[0].text, "live");
    }

    #[test]
    fn ingest_discards_fragment_delta_into_empty_dialog() {
        // A fresh install receiving deltas from a long-running origin: nothing
        // held, delta selected against a non-zero watermark — a fragment with
        // the entire history missing below it.
        let dev = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::Assistant, "floating", 70)])], None, &no_persisted(), 50, 100, String::new());
        assert!(dev.sessions[0].dialog.is_empty(), "fragment discarded; catch-up fills on history open");
    }

    #[test]
    fn ingest_discards_non_contiguous_delta_above_held() {
        let mut persisted = HashMap::new();
        persisted.insert("laptop/proj".to_string(), vec![entry(DialogRole::User, "u1", 10)]);
        // Held reaches 10, delta covers (50, now] — entries in (10, 50] would
        // be silently missing if this merged.
        let dev = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::Assistant, "gapped", 70)])], None, &persisted, 50, 100, String::new());
        let dialog = &dev.sessions[0].dialog;
        assert_eq!(dialog.len(), 1, "held prefix kept, gapped delta discarded");
        assert_eq!(dialog[0].text, "u1");
    }

    #[test]
    fn ingest_accepts_full_dialog_into_empty() {
        // delta_from == 0: the delta is the complete dialog (fresh origin
        // pusher), accepted even with nothing held.
        let dev = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::User, "u1", 10)])], None, &no_persisted(), 0, 100, String::new());
        assert_eq!(dev.sessions[0].dialog.len(), 1);
    }

    #[test]
    fn ingest_clears_display_name_from_sender() {
        let mut item = push_item("proj", Vec::new());
        item.session.display_name = Some("sender name".into());
        let dev = ingest("laptop", vec![item], None, &no_persisted(), 0, 100, String::new());
        assert_eq!(dev.sessions[0].display_name, None, "receiver's custom names win");
    }

    // -------- build_push_chunk --------

    #[test]
    fn build_push_chunk_strips_dialog_and_selects_delta_by_watermark() {
        let sessions = vec![session(
            "proj",
            vec![entry(DialogRole::User, "old", 10), entry(DialogRole::User, "new", 100)],
        )];
        let chunk = build_push_chunk("desktop", 9078, &sessions, 50, 1000, usize::MAX);
        assert_eq!(chunk.push.device_name, "desktop");
        assert_eq!(chunk.push.listen_port, 9078);
        assert_eq!(chunk.push.delta_from, 50, "chunk declares the watermark it was built against");
        assert!(chunk.push.sessions[0].session.dialog.is_empty(), "dialog travels as delta only");
        assert_eq!(chunk.push.sessions[0].dialog_delta.len(), 1);
        assert_eq!(chunk.push.sessions[0].dialog_delta[0].text, "new");
        assert!(chunk.drained);
        assert_eq!(chunk.ack_watermark, 1000 - WATERMARK_MARGIN_MS, "drained chunk acks to capture minus margin");
    }

    #[test]
    fn build_push_chunk_zero_watermark_sends_full_dialog() {
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "u", 10)])];
        let chunk = build_push_chunk("desktop", 9078, &sessions, 0, 1000, usize::MAX);
        assert_eq!(chunk.push.sessions[0].dialog_delta.len(), 1, "first push after start is a full sync");
        assert_eq!(chunk.push.delta_from, 0);
    }

    #[test]
    fn build_push_chunk_budget_cuts_oldest_first_and_next_chunk_continues() {
        // Three entries of 4 text bytes each; budget 8 fits exactly two.
        let sessions = vec![session(
            "proj",
            vec![entry(DialogRole::User, "aaaa", 10), entry(DialogRole::User, "bbbb", 20), entry(DialogRole::User, "cccc", 30)],
        )];
        let first = build_push_chunk("desktop", 9078, &sessions, 0, 1000, 8);
        assert!(!first.drained);
        let texts: Vec<&str> = first.push.sessions[0].dialog_delta.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["aaaa", "bbbb"], "oldest entries first — backfill grows upward from the watermark");
        assert_eq!(first.ack_watermark, 20, "partial chunk acks its last included timestamp");

        let second = build_push_chunk("desktop", 9078, &sessions, first.ack_watermark, 1000, 8);
        assert!(second.drained);
        assert_eq!(second.push.delta_from, 20, "next chunk is contiguous with the previous ack");
        assert_eq!(second.push.sessions[0].dialog_delta.len(), 1);
        assert_eq!(second.push.sessions[0].dialog_delta[0].text, "cccc");
    }

    #[test]
    fn build_push_chunk_never_splits_equal_timestamps() {
        // Two sessions with entries at the same timestamp: the budget cut
        // would land between them, but a `> watermark` selection would then
        // skip the twin forever — both must ride in the same chunk.
        let sessions = vec![
            session("a", vec![entry(DialogRole::User, "aaaa", 10)]),
            session("b", vec![entry(DialogRole::User, "bbbb", 10), entry(DialogRole::User, "cccc", 20)]),
        ];
        let chunk = build_push_chunk("desktop", 9078, &sessions, 0, 1000, 4);
        assert!(!chunk.drained);
        assert_eq!(chunk.ack_watermark, 10);
        let total: usize = chunk.push.sessions.iter().map(|s| s.dialog_delta.len()).sum();
        assert_eq!(total, 2, "both ts=10 twins included despite the budget");
    }

    #[test]
    fn build_push_chunk_oversized_first_entry_still_progresses() {
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "this text exceeds the tiny budget", 10)])];
        let chunk = build_push_chunk("desktop", 9078, &sessions, 0, 1000, 4);
        assert!(chunk.drained, "single entry over budget is sent anyway");
        assert_eq!(chunk.push.sessions[0].dialog_delta.len(), 1);
    }

    #[test]
    fn build_push_chunk_empty_backlog_is_drained_heartbeat() {
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "u", 10)])];
        let chunk = build_push_chunk("desktop", 9078, &sessions, 50, 1000, 8);
        assert!(chunk.drained);
        assert!(chunk.push.sessions[0].dialog_delta.is_empty(), "metadata-only heartbeat");
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
