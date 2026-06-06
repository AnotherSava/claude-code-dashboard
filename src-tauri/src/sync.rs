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
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;

use crate::commands::{emit_sessions_updated_remote, now_ms};
use crate::config::ConfigState;
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
/// drop out by not being carried), and merge each delta.
fn ingest(device: &str, sessions: Vec<SessionSync>, prev: Option<&RemoteDevice>, now: i64, origin_addr: String) -> RemoteDevice {
    let mut out = Vec::with_capacity(sessions.len());
    for item in sessions {
        let mut s = item.session;
        s.id = format!("{device}/{}", s.id);
        s.origin = Some(device.to_string());
        s.display_name = None; // receiver's custom names win at emit time
        let mut dialog = prev
            .and_then(|p| p.sessions.iter().find(|ps| ps.id == s.id))
            .map(|ps| ps.dialog.clone())
            .unwrap_or_default();
        merge_dialog_entries(&mut dialog, &item.dialog_delta);
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
    tracing::debug!(
        device = %push.device_name,
        sessions = push.sessions.len(),
        delta_entries = push.sessions.iter().map(|s| s.dialog_delta.len()).sum::<usize>(),
        "sync push received"
    );
    let origin_addr = format!("http://{}:{}", addr.ip(), push.listen_port);
    let now = now_ms();
    {
        let mut remote = state.remote.lock().unwrap();
        let prev = remote.get(&push.device_name);
        let device = ingest(&push.device_name, push.sessions, prev, now, origin_addr);
        remote.insert(push.device_name.clone(), device);
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

/// Build one peer's push payload: full metadata for every local session,
/// dialog entries newer than the peer's watermark.
fn build_push(device_name: &str, listen_port: u16, sessions: &[AgentSession], watermark: i64) -> SyncPush {
    SyncPush {
        device_name: device_name.to_string(),
        listen_port,
        sessions: sessions
            .iter()
            .map(|s| {
                let mut meta = s.clone();
                let dialog_delta = meta.dialog.iter().filter(|e| e.timestamp > watermark).cloned().collect();
                meta.dialog = Vec::new();
                SessionSync { session: meta, dialog_delta }
            })
            .collect(),
    }
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
    for peer in &cfg.sync.peers {
        let watermark = watermarks.get(peer).copied().unwrap_or(0);
        let payload = build_push(&cfg.sync.device_name, cfg.sync.listen_port, &sessions, watermark);
        let url = format!("{}/api/sync", peer.trim_end_matches('/'));
        match client.post(&url).bearer_auth(&token).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                watermarks.insert(peer.clone(), capture - WATERMARK_MARGIN_MS);
            }
            // Failures leave the watermark in place, so the next successful
            // push carries everything the peer missed. Offline peers are
            // routine — log at debug, not warn.
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
/// the owning device, the raw id on the origin (prefix stripped), the origin
/// address, and `since` — the max dialog timestamp already held (0 when
/// nothing is held, which fetches the full dialog). `None` for local ids:
/// no remote device prefix matches.
fn resolve_fetch_target(
    remote: &std::collections::BTreeMap<String, RemoteDevice>,
    session_id: &str,
) -> Option<(String, String, String, i64)> {
    remote.iter().find(|(d, _)| session_id.starts_with(&format!("{d}/"))).map(|(d, dev)| {
        let since = dev
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .and_then(|s| s.dialog.iter().map(|e| e.timestamp).max())
            .unwrap_or(0);
        (d.clone(), session_id[d.len() + 1..].to_string(), dev.origin_addr.clone(), since)
    })
}

/// Catch-up fetch for one remote session's dialog, triggered when the history
/// window targets it. `since` needs no bookkeeping — it's the max timestamp
/// we already hold (0 after a restart, which fetches the full dialog).
/// Fire-and-forget: on failure (origin offline) the window simply shows
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
        let Some((device, raw_id, origin_addr, since)) = resolve_fetch_target(&state.remote.lock().unwrap(), &session_id) else {
            return;
        };
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("reqwest client");
        let url = format!("{origin_addr}/api/sync/dialog");
        let resp = client
            .get(&url)
            .query(&[("id", raw_id.as_str()), ("since", since.to_string().as_str())])
            .bearer_auth(&token)
            .send()
            .await;
        let entries: Vec<DialogEntry> = match resp {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::debug!(url = %url, error = %e, "dialog catch-up parse failed");
                    return;
                }
            },
            Ok(r) => {
                tracing::debug!(url = %url, status = %r.status(), "dialog catch-up rejected");
                return;
            }
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "dialog catch-up failed");
                return;
            }
        };
        if entries.is_empty() {
            return;
        }
        tracing::debug!(session = %session_id, since, entries = entries.len(), "dialog catch-up merged");
        {
            let mut remote = state.remote.lock().unwrap();
            let Some(dev) = remote.get_mut(&device) else {
                return;
            };
            let Some(s) = dev.sessions.iter_mut().find(|s| s.id == session_id) else {
                return;
            };
            if since == 0 {
                // Nothing held — the response is the full dialog as the
                // origin knows it; take it wholesale.
                s.dialog = entries;
            } else {
                merge_dialog_entries(&mut s.dialog, &entries);
            }
        }
        emit_sessions_updated_remote(&app);
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

    #[test]
    fn ingest_namespaces_and_stamps_origin() {
        let dev = ingest("laptop", vec![push_item("proj", Vec::new())], None, 100, "http://1.2.3.4:9078".into());
        assert_eq!(dev.sessions.len(), 1);
        assert_eq!(dev.sessions[0].id, "laptop/proj");
        assert_eq!(dev.sessions[0].origin.as_deref(), Some("laptop"));
        assert_eq!(dev.last_seen, 100);
        assert_eq!(dev.origin_addr, "http://1.2.3.4:9078");
    }

    #[test]
    fn ingest_accumulates_dialog_across_pushes() {
        let first = ingest("laptop", vec![push_item("proj", vec![entry(DialogRole::User, "u1", 10)])], None, 100, String::new());
        let second = ingest(
            "laptop",
            vec![push_item("proj", vec![entry(DialogRole::Assistant, "a1", 20)])],
            Some(&first),
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
            100,
            String::new(),
        );
        let second = ingest("laptop", vec![push_item("alive", Vec::new())], Some(&first), 200, String::new());
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].id, "laptop/alive");
    }

    #[test]
    fn ingest_replayed_delta_does_not_duplicate() {
        let delta = vec![entry(DialogRole::User, "u1", 10), entry(DialogRole::Assistant, "a1", 20)];
        let first = ingest("laptop", vec![push_item("proj", delta.clone())], None, 100, String::new());
        let second = ingest("laptop", vec![push_item("proj", delta)], Some(&first), 200, String::new());
        assert_eq!(second.sessions[0].dialog.len(), 2);
    }

    #[test]
    fn ingest_clears_display_name_from_sender() {
        let mut item = push_item("proj", Vec::new());
        item.session.display_name = Some("sender name".into());
        let dev = ingest("laptop", vec![item], None, 100, String::new());
        assert_eq!(dev.sessions[0].display_name, None, "receiver's custom names win");
    }

    // -------- build_push --------

    #[test]
    fn build_push_strips_dialog_and_selects_delta_by_watermark() {
        let sessions = vec![session(
            "proj",
            vec![entry(DialogRole::User, "old", 10), entry(DialogRole::User, "new", 100)],
        )];
        let push = build_push("desktop", 9078, &sessions, 50);
        assert_eq!(push.device_name, "desktop");
        assert_eq!(push.listen_port, 9078);
        assert!(push.sessions[0].session.dialog.is_empty(), "dialog travels as delta only");
        assert_eq!(push.sessions[0].dialog_delta.len(), 1);
        assert_eq!(push.sessions[0].dialog_delta[0].text, "new");
    }

    #[test]
    fn build_push_zero_watermark_sends_full_dialog() {
        let sessions = vec![session("proj", vec![entry(DialogRole::User, "u", 10)])];
        let push = build_push("desktop", 9078, &sessions, 0);
        assert_eq!(push.sessions[0].dialog_delta.len(), 1, "first push after start is a full sync");
    }

    // -------- resolve_fetch_target --------

    #[test]
    fn resolve_fetch_target_parses_namespaced_id_and_since() {
        let mut remote = std::collections::BTreeMap::new();
        let mut s = session("laptop/proj", vec![entry(DialogRole::User, "u", 10), entry(DialogRole::Assistant, "a", 70)]);
        s.origin = Some("laptop".into());
        remote.insert("laptop".to_string(), RemoteDevice { sessions: vec![s], last_seen: 0, origin_addr: "http://1.2.3.4:9078".into() });
        let (device, raw_id, addr, since) = resolve_fetch_target(&remote, "laptop/proj").expect("target");
        assert_eq!(device, "laptop");
        assert_eq!(raw_id, "proj");
        assert_eq!(addr, "http://1.2.3.4:9078");
        assert_eq!(since, 70, "max held timestamp");
    }

    #[test]
    fn resolve_fetch_target_returns_zero_since_when_nothing_held() {
        let mut remote = std::collections::BTreeMap::new();
        remote.insert("laptop".to_string(), RemoteDevice { sessions: vec![session("laptop/proj", Vec::new())], last_seen: 0, origin_addr: String::new() });
        let (_, _, _, since) = resolve_fetch_target(&remote, "laptop/proj").expect("target");
        assert_eq!(since, 0, "empty dialog fetches everything");
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
