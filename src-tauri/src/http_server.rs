use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use std::net::SocketAddr;
use tauri::{AppHandle, Manager};

use crate::adapters::{self, AdapterOutput};
use crate::commands::{emit_sessions_updated, now_ms};
use crate::config::ConfigState;
use crate::log_watcher::WatcherRegistry;
use crate::prompt_history::PromptHistoryStore;
use crate::state::AppState;

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
}

async fn post_event(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(req): Json<EventRequest>,
) -> StatusCode {
    // CSRF guard: block browser-originated requests. urllib / curl don't send
    // Origin; browser XHRs do. "null" is allowed (file:// / data:).
    if let Some(origin) = headers.get("origin") {
        match origin.to_str() {
            Ok("null") => {}
            _ => return StatusCode::FORBIDDEN,
        }
    }

    let Some(state) = app.try_state::<AppState>() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let cfg = cfg_state.snapshot();

    let output = adapters::dispatch(&req.client, &req.event, &req.payload, &cfg);

    match output {
        AdapterOutput::Set { input, transcript_path } => {
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %input.id,
                status = ?input.status,
                label = ?input.label,
                "event -> set"
            );
            let chat_id = input.id.clone();
            let history = app.try_state::<PromptHistoryStore>();
            let restored = history.as_ref().and_then(|h| h.get(&chat_id));
            let dialog_changed = state.apply_set(input, now_ms(), &cfg.continuation_prompts, restored);
            if dialog_changed {
                if let Some(ref h) = history {
                    let sessions = state.sessions.lock().unwrap();
                    if let Some(s) = sessions.iter().find(|s| s.id == chat_id) {
                        h.save_session(s);
                    }
                    drop(sessions);
                    h.save_to_disk();
                }
            }
            if let Some(tp) = transcript_path {
                if let Some(reg) = app.try_state::<WatcherRegistry>() {
                    reg.start(app.clone(), chat_id, tp);
                }
            }
            emit_sessions_updated(&app);
        }
        AdapterOutput::Clear { id } => {
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %id,
                "event -> clear"
            );
            state.apply_clear(&id);
            if let Some(reg) = app.try_state::<WatcherRegistry>() {
                reg.stop(&id);
            }
            emit_sessions_updated(&app);
        }
        AdapterOutput::Ignore => {
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                "event -> ignored"
            );
        }
    }
    StatusCode::NO_CONTENT
}
