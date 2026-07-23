use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tauri::{AppHandle, Manager};

use crate::adapters::{self, AdapterOutput};
use crate::chat_id_registry::ChatIdRegistry;
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

async fn post_event(
    State(app): State<AppHandle>,
    headers: HeaderMap,
    Json(req): Json<EventRequest>,
) -> (StatusCode, Json<EventResponse>) {
    // CSRF guard: block browser-originated requests. urllib / curl don't send
    // Origin; browser XHRs do. "null" is allowed (file:// / data:).
    if let Some(origin) = headers.get("origin") {
        match origin.to_str() {
            Ok("null") => {}
            _ => return (StatusCode::FORBIDDEN, Json(EventResponse::default())),
        }
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
            // On SessionStart, mint the session's rotating nonce and hand the hook
            // the instruction to inject; on Stop, a dropped marker on the settled
            // turn's final message flags orthogonal drift (status is untouched).
            if cfg.instruction_canary_enabled {
                if req.event == "SessionStart" {
                    if let Some(ns) = app.try_state::<crate::nonce_store::NonceStore>() {
                        let nonce = ns.mint(&chat_id, now);
                        let marker = crate::adapters::claude::marker_for(&cfg.instruction_canary_marker, &nonce);
                        resp.additional_context = Some(format!(
                            "Adherence check for this session: end every response you write with the exact text {marker} \
                             — a hidden marker, so do not mention, explain, or alter it."
                        ));
                    }
                } else if req.event == "Stop" {
                    // Judged only when this session has a nonce and produced a final
                    // message; a tool-only / empty-final turn is exempt (left as-is).
                    let final_msg = req.payload.get("last_assistant_message").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
                    if let (Some(final_msg), Some(ns)) = (final_msg, app.try_state::<crate::nonce_store::NonceStore>()) {
                        if let Some((nonce, seen)) = ns.get(&chat_id) {
                            let marker = crate::adapters::claude::marker_for(&cfg.instruction_canary_marker, &nonce);
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
                            let drifted = !present && seen;
                            let reason = if present {
                                "adherence marker present"
                            } else if seen {
                                "final message dropped the marker after prior adherence"
                            } else {
                                "marker absent but never confirmed (instruction may be undelivered); holding"
                            };
                            let changed = state.set_drift(&chat_id, drifted, now);
                            tracing::debug!(chat_id = %chat_id, decision = "drift_check", drifted, seen, changed, marker = %marker, reason, "canary drift check");
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
            // Drop the session's canary nonce so a `/clear`-recreated row (same
            // cwd-derived chat_id) mints a fresh one on its next SessionStart.
            if let Some(ns) = app.try_state::<crate::nonce_store::NonceStore>() {
                ns.forget(&id);
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
