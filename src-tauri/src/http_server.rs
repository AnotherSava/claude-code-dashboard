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
use crate::nonce_store::NonceStore;
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
}
