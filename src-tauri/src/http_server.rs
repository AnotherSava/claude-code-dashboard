use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use tauri::{AppHandle, Emitter, Manager};

use crate::adapters::{self, AdapterOutput, SubagentEffect};
use crate::chat_id_registry::ChatIdRegistry;
use crate::commands::{emit_sessions_updated, now_ms, remove_session, resolved_snapshot, LeaveVia};
use crate::config::ConfigState;
use crate::log_watcher::{Graft, WatcherRegistry};
use crate::membership::{self, Admission, Departure, EventFacts, Left, Members, Membership};
use crate::nonce_store::NonceStore;
use crate::peer_message::{self, Outcome, Receipt};
use crate::prompt_history::PromptHistoryStore;
use crate::session_launcher;
use crate::start_approval;
use crate::session_registry::{Activity, LiveSession, SessionRegistry};
use crate::state::{AgentSession, AppState, BoundaryKind, SettleScope, Status};
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
        .route("/api/window", post(post_window))
        .route("/api/session-clean", post(post_session_clean))
        .route("/api/project/rename", post(post_project_rename))
        .with_state(app);

    if let Err(e) = axum::serve(listener, router).await {
        tracing::error!(error = %e, "http serve ended");
    }
}

/// What a `/pull` run reports to `POST /api/session-clean`.
#[derive(Deserialize)]
struct SessionCleanRequest {
    /// The Claude session the claim is about. Required: a `cwd` alone cannot
    /// separate two sessions open on one repo, which is exactly the case the
    /// relay already refuses as `ambiguous_target`, and crediting the wrong one
    /// would hide a sibling's unfinished work.
    #[serde(default)]
    session_id: String,
    /// Where that session is running, used only to derive the row id for a
    /// session this dashboard has not anchored yet.
    #[serde(default)]
    cwd: Option<String>,
}

/// What it is told back. Nothing reads this in production — `session_clean.py`
/// closes the response unread, by design — so it exists for a human holding
/// `curl` and for the tests, which is why it says *why* rather than just whether.
#[derive(Serialize, Default)]
struct SessionCleanResponse {
    recorded: bool,
    reason: &'static str,
}

/// Record a `/pull` run's report that it left nothing worth coming back to.
///
/// Records a *claim* and settles nothing: `pull_declared_clean` weighs it at the
/// turn's `Stop`, because `/pull` posts from inside the turn it is reporting on
/// and that turn's own `Stop` would overwrite any status set here moments later.
///
/// Gated on a loopback `Host` as well as the `Origin` check (`csrf_refusal`'s
/// `true`), like the roster and the message route and unlike `/api/event`: the
/// caller is a script on this machine with no host alias to support, so there is
/// nothing to lose by requiring it.
async fn post_session_clean(State(app): State<AppHandle>, headers: HeaderMap, Json(req): Json<SessionCleanRequest>) -> Response {
    if let Some(detail) = csrf_refusal(&headers, true) {
        return csrf_refused(detail).into_response();
    }
    // Takes a row lock, so it goes the way every other row write goes — off the
    // async workers, and surviving the caller hanging up.
    match tauri::async_runtime::spawn_blocking(move || apply_session_clean(&app, req)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!(error = %e, "session-clean handler failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(SessionCleanResponse::default())).into_response()
        }
    }
}

fn apply_session_clean(app: &AppHandle, req: SessionCleanRequest) -> Response {
    let answer = |status: StatusCode, recorded: bool, reason: &'static str| (status, Json(SessionCleanResponse { recorded, reason })).into_response();

    if req.session_id.is_empty() {
        return answer(StatusCode::BAD_REQUEST, false, "no session_id, and a cwd alone cannot say which session this is about");
    }
    let Some(state) = app.try_state::<AppState>() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(SessionCleanResponse::default())).into_response();
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(SessionCleanResponse::default())).into_response();
    };
    let cfg = cfg_state.snapshot();

    // `anchored`, never `resolve`: this is a read of an existing row's identity
    // and must not mint an anchor. A session with no anchor yet has had no hook
    // event, so it has no row for a claim to attach to either, and the `cwd`
    // derivation below is what finds the row in the ordinary case where the
    // anchor and the derivation agree.
    let chat_id = app
        .try_state::<ChatIdRegistry>()
        .and_then(|r| r.anchored(&req.session_id))
        .unwrap_or_else(|| adapters::claude::derive_chat_id(req.cwd.as_deref(), cfg.projects_root.as_deref()));

    let row_lock = app.try_state::<crate::commands::RowLocks>().map(|locks| locks.row(&chat_id));
    let _row_guard = row_lock.as_deref().map(crate::commands::RowLocks::hold);

    let main = app.try_state::<Members>().and_then(|m| m.main_session(&chat_id));
    if !clean_claim_permitted(main.as_deref(), &req.session_id) {
        tracing::info!(chat_id = %chat_id, decision = "pull_claim", outcome = "not_owner", "a pull reported a clean run for a row another session drives");
        return answer(StatusCode::CONFLICT, false, "another session drives that row, so this claim is not its to make");
    }

    if !state.record_clean_claim(&chat_id, now_ms()) {
        tracing::info!(chat_id = %chat_id, decision = "pull_claim", outcome = "no_row", "a pull reported a clean run for a session this dashboard holds no row for");
        return answer(StatusCode::NOT_FOUND, false, "no row here for that session");
    }
    tracing::info!(chat_id = %chat_id, decision = "pull_claim", outcome = "recorded", "a pull reported leaving nothing to come back to");
    answer(StatusCode::OK, true, "recorded; whether it settles CLEAN is decided at this turn's Stop")
}

/// What `POST /api/project/rename` is told: the project's folder before and
/// after the move. Paths rather than ids, so the dashboard derives both ids by
/// its own rule (`projects_root` included) and the caller cannot get it wrong.
#[derive(Deserialize)]
struct ProjectRenameRequest {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
}

/// Carry a project's dashboard data over to the id its renamed folder derives;
/// see [`crate::project_rename`]. Sent by the `move-project` skill's printed
/// steps once every session in the project has exited.
///
/// Loopback `Host` required, like the session-clean route: the caller is a
/// command typed on this machine, and the route rewrites stored history.
async fn post_project_rename(State(app): State<AppHandle>, headers: HeaderMap, Json(req): Json<ProjectRenameRequest>) -> Response {
    if let Some(detail) = csrf_refusal(&headers, true) {
        return csrf_refused(detail).into_response();
    }
    // Row locks and whole-file writes, so off the async workers like every
    // other row write.
    let result = tauri::async_runtime::spawn_blocking(move || crate::project_rename::rename_project(&app, &req.old_path, &req.new_path, now_ms())).await;
    match result {
        Ok(Ok(report)) => (StatusCode::OK, Json(serde_json::json!({"ok": true, "report": report}))).into_response(),
        Ok(Err(refusal)) => {
            let status = match refusal {
                crate::project_rename::RenameRefusal::EmptyPath => StatusCode::BAD_REQUEST,
                _ => StatusCode::CONFLICT,
            };
            tracing::info!(decision = "project_rename", outcome = ?refusal, "project rename refused; nothing was changed");
            (status, Json(serde_json::json!({"ok": false, "detail": refusal.detail()}))).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "project rename handler failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// What `POST /api/window` is being asked to do.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum WindowRequest {
    /// Reveal a window, un-minimizing it first. Not a toggle: a caller asking to
    /// see something must never hide it because it happened to be up already.
    /// `label` defaults to the widget; the other configured windows ("history",
    /// "intensity", "about") are reachable by name, which is what makes this a
    /// window API rather than a widget switch.
    Show {
        #[serde(default)]
        label: Option<String>,
    },
    Hide,
    /// Open the history window on a session, exactly as clicking its row does.
    History { id: String },
    /// Set a window's size. Part of window control, not a capture hook: an
    /// agent arranging the dashboard wants this for the same reasons a person
    /// dragging an edge does. `label` is Tauri's own ("main", "history", …).
    Resize { label: String, width: f64, height: f64 },
    /// Restore a window to filling the screen — the state `history` opens in,
    /// so a caller that resized one can put it back.
    Maximize { label: String },
    /// Place a window's top-left corner, in logical pixels from the top-left of
    /// the primary display. Completes the set: a caller that can show, size and
    /// maximize a window but not place one still has to ask a human to drag it.
    Move { label: String, x: f64, y: f64 },
    /// Open the Work intensity chart on a given week and view — the same shape
    /// as `History`, which names *which session* to show. `offset` counts weeks
    /// back from the current one (0 = this week, -1 = last), `view` is "day" or
    /// "week".
    ///
    /// This is a target selector, not the scroll position deliberately left out
    /// of this API: it says which data to render, exactly as `History` does, and
    /// is the only way to reach a week at all without a keypress.
    Intensity {
        #[serde(default)]
        offset: i32,
        #[serde(default)]
        view: Option<String>,
    },
}

/// Drive this dashboard's own windows from a local process.
///
/// It exists because an agent that needs the dashboard to *show* something had
/// no way to ask. What it replaces is worse in every dimension: synthesizing
/// mouse clicks at screen coordinates, which needs a system-wide Accessibility
/// grant, breaks whenever a row moves, and hands a general "control this Mac"
/// capability to whatever holds it. A named action on the app's own loopback API
/// needs no OS permission at all, says what it means, and keeps working when the
/// UI is redesigned.
///
/// It commands windows and nothing else — it cannot read a session, change
/// state, or start a turn. Scroll position, and anything else specific to
/// framing a screenshot, is deliberately absent: this is a control surface, not
/// a capture hook.
///
/// Gated like [`post_message`] rather than like the hook routes: a rebound page
/// reaching this could pop the widget open or bring a conversation on screen.
/// That is nuisance rather than exfiltration — the attacker cannot read the
/// result — but the stricter check costs one line, and "changes what is on the
/// user's screen" is a fair place to draw it. The hook route keeps the looser
/// gate because its `TAURI_DASHBOARD_URL` host alias is a real setup.
async fn post_window(State(app): State<AppHandle>, headers: HeaderMap, Json(req): Json<WindowRequest>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(detail) = csrf_refusal(&headers, true) {
        return csrf_refused(detail);
    }
    let acted = match &req {
        WindowRequest::Show { label } => {
            let label = label.as_deref().unwrap_or("main");
            if label == "main" {
                crate::commands::reveal_main(&app);
            } else {
                let Some(w) = app.get_webview_window(label) else {
                    return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "reason": "no_such_window"})));
                };
                let _ = w.show();
                let _ = w.set_focus();
            }
            "show"
        }
        WindowRequest::Hide => {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
            }
            "hide"
        }
        WindowRequest::History { id } => {
            if id.trim().is_empty() {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "reason": "empty_id"})));
            }
            // The widget is revealed first: a history window is opened *from* a
            // row, so a caller asking for one while the dashboard is hidden
            // almost certainly wants to see both.
            crate::commands::reveal_main(&app);
            if let Err(e) = crate::commands::open_history(id.clone(), app.clone()) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "reason": e})));
            }
            "history"
        }
        WindowRequest::Resize { label, width, height } => {
            let Some(w) = app.get_webview_window(label) else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "reason": "no_such_window"})));
            };
            // Logical pixels, so a caller asks for the size it means and the
            // same request lands identically on a Retina and a 1x display.
            if *width < 120.0 || *height < 80.0 {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "reason": "too_small"})));
            }
            let _ = w.unmaximize();
            if let Err(e) = w.set_size(tauri::LogicalSize::new(*width, *height)) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "reason": e.to_string()})));
            }
            "resize"
        }
        WindowRequest::Intensity { offset, view } => {
            let Some(w) = app.get_webview_window("intensity") else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "reason": "no_such_window"})));
            };
            if *offset > 0 {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "reason": "future_week"})));
            }
            let _ = w.show();
            let _ = w.set_focus();
            let _ = w.emit("intensity_target", serde_json::json!({"offset": offset, "view": view}));
            "intensity"
        }
        WindowRequest::Move { label, x, y } => {
            let Some(w) = app.get_webview_window(label) else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "reason": "no_such_window"})));
            };
            if let Err(e) = w.set_position(tauri::LogicalPosition::new(*x, *y)) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "reason": e.to_string()})));
            }
            "move"
        }
        WindowRequest::Maximize { label } => {
            let Some(w) = app.get_webview_window(label) else {
                return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "reason": "no_such_window"})));
            };
            let _ = w.maximize();
            "maximize"
        }
    };
    tracing::info!(decision = "window_command", action = acted, "drove a window from the local API");
    (StatusCode::OK, Json(serde_json::json!({"ok": true, "action": acted})))
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
    /// by the hook from its ancestor chain and reported fresh on every event. It
    /// keys the sender's membership in its row (`membership::Members`). It also
    /// picks the row ahead of the session's `ChatIdRegistry` anchor
    /// (`Members::row_of_pid`), except on the end signal, which is matched by
    /// session id because a process shutting down often has no resolvable pid.
    /// `liveness_reaper` judges each pid-keyed member by it. `None` (the hook
    /// couldn't identify the process, e.g. a node-based install, or a pre-field
    /// hook) admits the sender as a session-keyed member, which nothing reaps
    /// until a later event carries its pid and rekeys it.
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
///
/// `may_mint` is whether the starting session drives the row: the main, or any
/// member while none is elected (see `membership` and [`set_effects`]). Any
/// other session in the folder is handed the row's current nonce and never
/// mints, since a mint replaces the marker the main is emitting and its next
/// `Stop` would read as drift.
fn session_start_nonce(ns: &NonceStore, chat_id: &str, source: &str, now_ms: i64, may_mint: bool) -> Option<String> {
    // `fork` belongs with the other two and was missing: a `--fork-session`
    // start carries its parent's context *and* the marker instruction already in
    // it, so minting a fresh nonce there strands the row Pending on a marker the
    // model will never emit — the exact failure the paragraph above describes.
    if !may_mint || matches!(source, "resume" | "compact" | "fork") {
        ns.get(chat_id).map(|(nonce, _seen)| nonce)
    } else {
        Some(ns.mint(chat_id, now_ms))
    }
}

/// The marker instruction a `SessionStart` hands the hook to inject, if any.
fn canary_instruction(ns: &NonceStore, chat_id: &str, source: &str, now_ms: i64, may_mint: bool) -> Option<String> {
    let nonce = session_start_nonce(ns, chat_id, source, now_ms, may_mint)?;
    let marker = crate::adapters::claude::marker_for(crate::adapters::claude::CANARY_MARKER, &nonce);
    Some(format!(
        "Adherence check for this session: end every response you write with the exact text {marker}, \
         placed inline on the same line right after your final character (a single space before it, \
         no blank line) — a hidden marker, so do not mention, explain, or alter it."
    ))
}

/// Whether a `/pull` run's clean claim from `claimant` may land on a row whose
/// main session is `main`. A chat_id is cwd-derived, so every session in that
/// directory addresses one row; a claim from one that does not drive the row
/// would settle work the main has parked there — `status_before_working` keeps
/// its older `Idle` across a `Working` → `Working` prompt — and CLEAN then hides
/// it, the hiding-unread-work direction. Refused only where a main is known and
/// differs, so a row with no main (none elected, or nothing since a restart)
/// still accepts.
fn clean_claim_permitted(main: Option<&str>, claimant: &str) -> bool {
    main.is_none_or(|m| m == claimant)
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

/// Browser guard for every route on this server: refuse any request carrying an
/// `Origin` header, whatever its value. A browser attaches one to every request
/// whose method is not `GET` or `HEAD`, and to every cross-origin request made in
/// CORS mode; the hook, curl, urllib and PowerShell's web cmdlets send none. The
/// server is loopback-only and unauthenticated, so a page the user happens to
/// have open is the whole threat model.
///
/// **`Origin: null` is refused with the rest.** A page sends it whenever it
/// chooses — from a sandboxed iframe, or on a `POST` made with
/// `mode: "same-origin"` under `referrerPolicy: "no-referrer"` (Fetch, "append a
/// request `Origin` header") — so exempting it for the `file://` and `data:`
/// documents that also send it exempts the rebound page described below, and
/// nothing that calls this server is such a document.
///
/// **What it closes.** Cross-site writes, which axum's `Json` content-type check
/// and the CORS preflight this server never answers also stop, and DNS rebinding
/// on every `POST`: a page whose domain is rebound to `127.0.0.1` becomes
/// same-origin with this server, and its `POST` still carries its `Origin`.
/// **What it does not close** is rebinding on a `GET`, since a same-origin `GET`
/// carries no `Origin`. The only `GET` route, the roster, carries every project
/// name, status and label — prompt text included — so it also requires a
/// loopback `Host` through [`csrf_refusal`].
fn origin_blocked(headers: &HeaderMap) -> bool {
    headers.contains_key(axum::http::header::ORIGIN)
}

/// Why this server refuses a request as browser-originated or rebound, worded so
/// the caller knows what to change; `None` admits it. Every route applies the
/// `Origin` rule. Routes that start a turn on another machine, change what is on
/// the user's screen, or answer a `GET` (the roster, which the `Origin` rule
/// cannot protect) also pass `require_loopback_host`, a second gate that does not
/// depend on what the browser attaches. The cost there is that a
/// `TAURI_DASHBOARD_URL` naming the server by a non-loopback alias reaches the
/// hook route and no other. The hook route cannot take the gate, since that
/// alias is a supported setup for it and the `Origin` rule already covers a
/// `POST`.
///
/// Every handler calls this first rather than repeating the checks: the guard
/// is per-handler (there is no tower layer on this router), so a new route is
/// unprotected until it makes this call, and one function is one place to
/// change the rule.
fn csrf_refusal(headers: &HeaderMap, require_loopback_host: bool) -> Option<&'static str> {
    if origin_blocked(headers) {
        Some("a request carrying an Origin header is taken for a browser's and refused, whatever the value; send none, as curl, urllib and the hook do")
    } else if require_loopback_host && !host_is_loopback(headers) {
        Some("this route answers only a caller that names it by a loopback address or localhost in Host")
    } else {
        None
    }
}

/// The `403` body for a [`csrf_refusal`], in the shape the window route's other
/// answers already take. The message route carries the same detail inside its
/// receipt instead.
fn csrf_refused(detail: &'static str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok": false, "reason": "csrf", "detail": detail})))
}

/// Whether the request addressed us by a loopback name — the second gate the
/// message, window and roster routes carry through [`csrf_refusal`], closing DNS
/// rebinding there without depending on the browser attaching an `Origin`.
///
/// A rebound page reaches this server with the attacker's own hostname in
/// `Host`, because that is the name the browser resolved; a genuine local caller
/// has no reason to use anything but loopback. `localhost` is accepted because
/// it is a loopback name a local caller may type. A missing `Host`
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
    /// Live sessions Claude Code's own registry knows about and the hook stream
    /// does not — see `RegistryRow`. A project present in both arrays for the
    /// same device appears only in `agents`, so the name states the precedence
    /// rule.
    ///
    /// Covers **every** device, not just this one. It was local-only at first,
    /// and that made discovery strictly narrower than delivery: on 2026-08-30 a
    /// live session on the other machine was absent here while the relay could
    /// reach it perfectly well, so an agent following the documented "check the
    /// roster first" concluded the target was gone and gave up.
    registry_only: Vec<RegistryRow>,
    /// Devices whose live-session list could not be obtained: the registry was
    /// unreadable there, or the peer has not pushed one.
    ///
    /// A **list of devices rather than a flag**, because that is the shape that
    /// stays honest with more than one machine. This replaced a
    /// `registry_only: Option<…>` whose `None` meant "this device's registry was
    /// unreadable" — which could not survive the array covering several devices,
    /// since one unreadable machine would have had to null the whole array and
    /// hide every other machine's rows. Empty here means every known device
    /// answered; a device named here is one whose silence proves nothing.
    registry_unreadable: Vec<String>,
}

/// One synced peer dashboard. `sessions` counts the rows attributed to it in
/// `agents`, so a device that is live but idle is visible as itself.
#[derive(Serialize, Debug, PartialEq)]
struct PeerRow {
    device: String,
    last_seen_age_ms: i64,
    sessions: usize,
    /// Whether that device's *name* was corroborated by Tailscale on its last
    /// push (`attested`), or merely taken at its word (`claimed`).
    ///
    /// Reported because a check that silently succeeds is indistinguishable
    /// from one that silently no-ops — the failure mode this project has paid
    /// for before. `attested` is deliberately not called "verified": it is as
    /// good as the tailnet's ACLs and says nothing about the *agent* half of any
    /// sender's identity, which nothing can check.
    ///
    /// Never `mismatch`: such a push is refused and never becomes a row.
    identity: crate::tailnet::Attestation,
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
    /// Claude Code's own name for the session behind a local row — the address
    /// `ListAgents`/`SendMessage` use — read from the same registry join as
    /// `RegistryRow::name` and meaning the same. Distinct from `display_name`,
    /// which comes from this dashboard's rename store.
    ///
    /// Present only where exactly one live session backs the row (`sessions ==
    /// 1`) and that session has a name: with two in one directory either could
    /// be meant, and naming the freshest would hand a caller one of two
    /// addresses as if it were the only one. Never present on a remote row.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// How many live sessions in Claude Code's registry back this local row — 0
    /// where none does, 2 after a fork migration left two in one directory, 1
    /// where the one session has no name. Omitted on a remote row and wherever
    /// this machine's registry could not be read (which `registry_unreadable`
    /// then names), so a missing `name` always says which reason it has.
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions: Option<usize>,
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
/// Rows for a **peer's** registry ride the sync push and land here too, so this
/// array answers "does that session exist, and where" for the whole fleet.
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
    /// Withheld where `sessions > 1`, by `AgentRow::name`'s rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// `idle` / `busy` / `unknown`, the registry's own words. **Never a
    /// `status`**: idle/busy cannot express `blocked`, `waiting` or `error`, so
    /// mapping `busy -> working` would not be coarse but false — a session
    /// parked on a question is `blocked` here and `idle` there.
    activity: Activity,
    /// Time since the registry last wrote that activity. Omitted when the
    /// record carries no stamp.
    ///
    /// **Skew-free on both paths**, unlike `AgentRow::status_age_ms`. A local
    /// row measures it here; a remote row arrives as an age measured by the
    /// sender at push time and has this receiver's own since-the-push elapsed
    /// added to it. Two durations summed need no clock agreement, so there is no
    /// skew to disclose.
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_age_ms: Option<i64>,
    /// How many interactive sessions collapsed into this row (2 after a fork
    /// migration left two tabs in one directory).
    sessions: usize,
    /// Whether the session runs on this machine. Present because this array is
    /// no longer local-only.
    local: bool,
    /// How long since this row's device last pushed. Omitted for a local row,
    /// where a `0` would claim freshness on a channel that doesn't exist — the
    /// same rule `AgentRow` follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_age_ms: Option<i64>,
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
    remote_registry: &BTreeMap<String, Option<Vec<crate::sync::RegistrySync>>>,
    remote_identity: &BTreeMap<String, crate::tailnet::Attestation>,
    anchored: &dyn Fn(&str) -> Option<String>,
    last_seen: &BTreeMap<String, i64>,
    this_device: Option<&str>,
    sync_listening: bool,
    now_ms: i64,
) -> AgentsResponse {
    let age_since = |then: i64| (now_ms - then).max(0);

    // Each registry entry paired with the row it writes to, and the live
    // sessions summed per row. Summed rather than taken from one entry because
    // `live_rows` groups by the *current* cwd derivation while a row is keyed
    // by its anchor, so a session that has `cd`-ed and a sibling still at the
    // root are two entries landing on one row.
    let registry_rows: Option<Vec<(&LiveSession, String)>> = registry.map(|regs| regs.iter().map(|s| (s, s.row_id(anchored))).collect());
    let mut live_by_row: HashMap<&str, (usize, Option<&str>)> = HashMap::new();
    for (s, id) in registry_rows.iter().flatten() {
        let entry = live_by_row.entry(id.as_str()).or_default();
        entry.0 += s.sessions();
        entry.1 = s.name.as_deref();
    }
    let sole_name = |count: usize, name: Option<&str>| name.filter(|_| count == 1).map(str::to_string);

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
            let (sessions, name) = match (&s.origin, &registry_rows) {
                (None, Some(_)) => {
                    let (count, name) = live_by_row.get(s.id.as_str()).copied().unwrap_or_default();
                    (Some(count), sole_name(count, name))
                }
                _ => (None, None),
            };
            Some(AgentRow {
                id: s.id.clone(),
                project,
                device,
                local: s.origin.is_none(),
                display_name: s.display_name.clone(),
                name,
                sessions,
                status: s.status,
                // The row's own text, read by the one function the widget row
                // and the Telegram ping read, so a task
                // another agent began reads here as the sender's task too, and
                // no agent message's envelope is ever reported.
                label: s.primary_text().into_owned(),
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
    // Keyed by (device, project) rather than by id alone, now that the array
    // spans machines: deduping on the bare project would delete a peer's row
    // because *this* box happens to run the same project, which is precisely the
    // cross-machine evidence the roster exists to carry.
    let hook_rows: HashSet<(Option<&str>, &str)> = agents.iter().map(|a| (a.device.as_deref(), a.project.as_str())).collect();
    let mut registry_only: Vec<RegistryRow> = Vec::new();
    let mut registry_unreadable: Vec<String> = Vec::new();

    match registry_rows {
        Some(regs) => registry_only.extend(
            regs.into_iter()
                .filter(|(_, id)| !hook_rows.contains(&(this_device, id.as_str())))
                .map(|(s, id)| RegistryRow {
                    project: id.clone(),
                    id,
                    device: this_device.map(str::to_string),
                    name: sole_name(s.sessions(), s.name.as_deref()),
                    activity: s.activity,
                    activity_age_ms: s.activity_age_ms,
                    sessions: s.sessions(),
                    local: true,
                    last_seen_age_ms: None,
                }),
        ),
        None => registry_unreadable.push(this_device.unwrap_or("this device").to_string()),
    }

    for (device, rows) in remote_registry {
        // A device reaped between the two locks has no freshness number, and a
        // row without one is the thing this route exists never to emit — the
        // same rule the `agents` loop applies.
        let Some(seen) = last_seen.get(device) else { continue };
        let push_age = age_since(*seen);
        let Some(rows) = rows else {
            registry_unreadable.push(device.clone());
            continue;
        };
        registry_only.extend(
            rows.iter()
                .filter(|s| !hook_rows.contains(&(Some(device.as_str()), s.chat_id.as_str())))
                .map(|s| RegistryRow {
                    id: format!("{device}/{}", s.chat_id),
                    project: s.chat_id.clone(),
                    device: Some(device.clone()),
                    name: sole_name(s.sessions, s.name.as_deref()),
                    activity: s.activity,
                    // Two durations summed: the sender's age at push time, plus
                    // how long ago that push arrived here. No clock agreement
                    // needed, so unlike `status_age_ms` this carries no skew.
                    activity_age_ms: s.activity_age_ms.map(|age| age + push_age),
                    sessions: s.sessions,
                    local: false,
                    last_seen_age_ms: Some(push_age),
                }),
        );
    }

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
            // A device present in `last_seen` but absent here was reaped between
            // the reads; `Claimed` is the safe reading, never `Attested`.
            identity: remote_identity.get(device).copied().unwrap_or(crate::tailnet::Attestation::Claimed),
        })
        .collect();

    AgentsResponse { device: this_device.map(str::to_string), sync_listening, peers, agents, registry_only, registry_unreadable }
}

/// Read-only roster of every session this dashboard tracks, local and synced.
/// Whether a `SessionStart` that resumed a conversation may claim
/// [`Status::Idle`] — the one CLEAN verdict `adapters::claude` cannot reach,
/// because it is a fact about the persisted dialog rather than about the event.
///
/// A resumed session is clean exactly when the conversation it resumed ended at
/// a `/clear`. That is narrower than "ended at a boundary" on purpose: every
/// removal appends a separator, so the weaker test reads true after a
/// compaction, after an ordinary exit, and after the liveness reaper — and
/// sessions here are started with `--continue`, which would make nearly every
/// row on the machine claim to be clean at start-up.
///
/// Scoped to `resume` alone. `clear` and `startup` are already answered by the
/// adapter from evidence that needs no history, and letting this widen to them
/// would make a `/clear` on a row with no persisted dialog fail to be clean.
fn resume_is_clean(event: &str, source: Option<&str>, dialog: Option<&[crate::state::DialogEntry]>) -> bool {
    event == "SessionStart" && source == Some("resume") && dialog.is_some_and(crate::state::ends_with_clear_boundary)
}

/// Why a claim did not settle CLEAN, or `None` where it did.
///
/// A verdict rather than a bool because the three facts fail for different
/// reasons and the log has to say which: a `pull_claim` with no `pull_clean` after
/// it marks a refusal, but not what refused it, and reconstructing that from the
/// neighbouring `classify` line is what diagnosing the first real claim cost.
/// `None` where no claim was outstanding at all — the overwhelmingly common case,
/// which logs nothing.
#[derive(Debug, PartialEq, Eq)]
enum CleanRefusal {
    /// The turn settled as anything but `Done` — canonically BLOCK or WAIT, each
    /// of which says something is still outstanding.
    StillOutstanding(Status),
    /// A human typed the prompt that began this turn.
    NotRelayed,
    /// The session had work parked in it when the request arrived.
    NotCleanBefore(Status),
}

impl CleanRefusal {
    fn slug(&self) -> &'static str {
        match self {
            Self::StillOutstanding(_) => "still_outstanding",
            Self::NotRelayed => "not_relayed",
            Self::NotCleanBefore(_) => "not_clean_before",
        }
    }
}

/// Whether a settling turn may claim [`Status::Idle`] because a peer asked this
/// session to pull and the pull left nothing behind — the third CLEAN source
/// named in `Status::Idle`'s own documentation.
///
/// Three independent facts have to agree, and each is owned by whoever can
/// actually establish it:
///
/// 1. **The run left nothing to come back to** — `claimed`, which only the
///    `/pull` skill knows, because only it saw whether anything conflicted and
///    whether anything was left for the user. It posts that judgment; this
///    dashboard never second-guesses it.
/// 2. **A peer asked, rather than the human** — `turn_from_relay`, which only
///    this dashboard knows, because it minted the preamble the arriving prompt
///    carries. It is here because the user's rule is about servicing *someone
///    else's* request: a session the human drove to a clean tree is finished
///    work they may well want to see, and CLEAN hides a row.
/// 3. **The session was already clean when the request arrived** —
///    `status_before_working`, the user's own "if it was in clean state before
///    this request". A pull that interrupts real work leaves that work sitting
///    there, so the row is not clean however little the pull itself did.
///
/// # Why only `Done`
///
/// `Blocked` and `Waiting` are refused outright rather than being folded into the
/// test. Both say something is still outstanding — a question on screen, work
/// still running — and no claim about a pull can speak to either. (`/pull` step 10
/// declines to signal where the *turn* goes on to ask the user anything, not
/// merely where the pull itself did, so the two rules agree. This one does not
/// rely on that: a refusal here is free, and the skill's discipline is not ours
/// to depend on. The case that settled the wording — a `/pull` nested inside
/// `/commit`, posting a claim 46 seconds before the commit plan's own approval
/// question — was refused here first.)
///
/// `Idle` → `Idle` is not special-cased: a row already clean needs no upgrade,
/// and the `Done` gate declines it for free.
fn pull_declared_clean(settling: Status, facts: Option<crate::state::CleanClaimFacts>) -> Result<(), Option<CleanRefusal>> {
    let Some(f) = facts.filter(|f| f.claimed) else { return Err(None) };
    if settling != Status::Done {
        return Err(Some(CleanRefusal::StillOutstanding(settling)));
    }
    if !f.turn_from_relay {
        return Err(Some(CleanRefusal::NotRelayed));
    }
    if f.status_before_working != Status::Idle {
        return Err(Some(CleanRefusal::NotCleanBefore(f.status_before_working)));
    }
    Ok(())
}

/// Mutates nothing — no `apply_set`, no emit, no `SyncDirty` poke, no store
/// write — which is also why it writes no `decision` line: every one of those
/// tags marks a state change and the `investigate` skill replays them to
/// reconstruct a row, so a polling reader would bury the real decisions under
/// entries that explain no state.
async fn get_agents(
    State(app): State<AppHandle>,
    headers: HeaderMap,
) -> Result<Json<AgentsResponse>, Response> {
    if let Some(detail) = csrf_refusal(&headers, true) {
        return Err(csrf_refused(detail).into_response());
    }
    let Some(state) = app.try_state::<AppState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
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
    Ok(Json(agent_roster(&sessions, registry.as_deref(), &state.remote_registry(), &state.remote_identity(), &|sid| chat_ids.as_ref().and_then(|r| r.anchored(sid)), &last_seen, this_device, sync_listening, now)))
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
    /// The caller's own one-line account of **why it is writing**, in its own
    /// words. Deliberately a separate field from the envelope's `from_task`,
    /// which is what this dashboard has on record for the row the caller named:
    /// the two are different facts of different strengths, and one field holding
    /// either would need a flag to say which it currently held.
    #[serde(default)]
    from_label: Option<String>,
    /// Set when this send answers a message that arrived here, echoing the
    /// `message_id` the envelope showed. Passed straight through; nothing in the
    /// dashboard branches on it.
    #[serde(default)]
    in_reply_to: Option<String>,
}

/// The task this dashboard has on record for the row the caller named.
///
/// **This corroborates a claim; it does not observe the sender.** `from_agent`
/// is chosen by the caller — the route is loopback and unauthenticated — so the
/// row looked up here is the one the caller *named*, which is its own row only
/// as far as that name is right. A caller that misnames itself gets another
/// row's task, which is why neither this function's result nor the envelope line
/// built from it is ever worded as something the dashboard saw. What makes the
/// value worth sending anyway is that the alternative is nothing: the receiving
/// row otherwise has no text of its own at all.
///
/// Attesting the loopback caller's process would turn the claimed half into an
/// observed one, and is the subject of its own memo; nothing here changes shape
/// when it arrives.
///
/// `None` wherever there is no answer — an unidentified caller, a name no local
/// row carries, a row that has no prompt recorded yet — never a substitute.
/// Remote rows are skipped: a row this device merely syncs is some other
/// machine's, so its prompt is not ours to report as a local sender's.
///
/// The row's task as a person gave it ([`AgentSession::person_task`]): where
/// another agent began the sender's own task, the task behind that, so the
/// receiver is told what a person asked for rather than handed an envelope to
/// unwrap. Where that task was never resolved, `None` — the message line the
/// row shows in its place is another agent's words, not a task, and sent on as
/// one it would be reported as this sender's task by the receiving row.
fn sender_task(rows: &[AgentSession], from_agent: &str) -> Option<String> {
    if from_agent == "unknown" {
        return None;
    }
    rows.iter()
        .find(|s| s.origin.is_none() && s.id == from_agent)
        .and_then(AgentSession::person_task)
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
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
    if let Some(detail) = csrf_refusal(&headers, true) {
        return refused("csrf", detail.into(), StatusCode::FORBIDDEN, None);
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
    //
    // The rows are kept rather than reduced to ids on the spot, because
    // `sender_task` needs one of them further down and a second snapshot would
    // be a second answer to the same question.
    let rows = resolved_snapshot(&app);
    let mut local_ids: Vec<String> = rows.iter().filter(|s| s.origin.is_none()).map(|s| s.id.clone()).collect();
    if let Some(registry) = app.try_state::<SessionRegistry>() {
        if let Some(live) = registry.live_sessions(cfg.projects_root.as_deref(), now) {
            local_ids.extend(live.into_iter().map(|s| s.chat_id));
        }
    }
    let last_seen = state.remote_last_seen();
    let devices: Vec<String> = last_seen.keys().cloned().collect();

    let (device, project) = match peer_message::resolve_message_target(&req.target, &local_ids, &devices, this_device) {
        peer_message::TargetResolution::Remote { device, project } => (device, project),
        peer_message::TargetResolution::LocalLive => {
            return refused(
                "local_target",
                "this session is on this machine; use Claude Code's `SendMessage`, which carries a kernel-verified sender identity and a reply address this route destroys".into(),
                StatusCode::BAD_REQUEST,
                this_device,
            );
        }
        // Nothing is running for a project on this machine. `SendMessage` is
        // still the right way to talk to a local agent — but it can only address
        // one that exists, so here it is not an option at all. The dashboard
        // supplies the part Claude Code cannot (a session), and then stands
        // aside: the message is **not** relayed, because relaying locally is
        // exactly what was refused above and starting a session does not change
        // why. The caller sends its own, with its own verified identity, once
        // the session is up.
        peer_message::TargetResolution::LocalIdle { project } => {
            // The same gate the relay hop checks first. A grant that half its
            // callers skip is not a grant — a machine whose owner left
            // `accept_messages` off, believing nothing can start here, must not
            // have a session started by anything that can reach loopback.
            if !cfg.sync.accept_messages {
                return refused(
                    "messages_not_accepted",
                    "this device has sync.accept_messages off, so it will not start a session on request".into(),
                    StatusCode::FORBIDDEN,
                    this_device,
                );
            }
            let (Some(guard), Some(registry)) = (app.try_state::<session_launcher::StartGuard>(), app.try_state::<SessionRegistry>()) else {
                return refused("state_unavailable", "the dashboard is still starting".into(), StatusCode::INTERNAL_SERVER_ERROR, this_device);
            };
            // Shared with the relay hop rather than reimplemented, because
            // every step of that sequence — the fresh re-confirmation of the
            // absence, the claim held across the wait, the cleanup of a session
            // that never got a surface — is a way to end up with two agents in
            // one directory, and two copies of it would drift.
            let startable = app.try_state::<crate::auto_start_store::AutoStartStore>().map(|s| s.snapshot()).unwrap_or_default();
            return match session_launcher::start_and_wait(&project, &startable, cfg.projects_root.as_deref(), &registry, &guard, now).await {
                session_launcher::StartResult::Refused(r) => {
                    let receipt = Receipt::new(Outcome::Refused, "", &req.target, this_device).because(r.slug()).detailed(r.detail());
                    refused(r.slug(), r.detail().into(), crate::sync::receipt_status(&receipt), this_device)
                }
                // Reached when the absence turned out to be stale: a session was
                // already there, so this is the ordinary local refusal after all.
                session_launcher::StartResult::Settled { started: false, .. } => refused(
                    "local_target",
                    "this session is on this machine; use Claude Code's `SendMessage`, which carries a kernel-verified sender identity and a reply address this route destroys".into(),
                    StatusCode::BAD_REQUEST,
                    this_device,
                ),
                session_launcher::StartResult::Settled { inbox, started: true } => {
                    let reached = !matches!(inbox, crate::session_registry::InboxLookup::NotFound);
                    tracing::info!(chat_id = %project, decision = "peer_start", from_agent, reached_inbox = reached, "started a local terminal session for a project with none");
                    // `Refused` is the honest outcome: nothing was relayed, and
                    // nothing will be. The relay was declined for the same
                    // reason a live local target is declined — `SendMessage`
                    // carries an identity this route destroys — and starting a
                    // session does not change that. The status comes from the
                    // canonical map so it cannot contradict the outcome, which
                    // a `202` did.
                    let r = Receipt::new(Outcome::Refused, "", &req.target, this_device)
                        .because("local_target_started")
                        .detailed(if reached {
                            "nothing was running for that project, so a terminal session was started for it and is now reachable; nothing was relayed — send to it with Claude Code's `SendMessage`"
                        } else {
                            "nothing was running for that project, so a terminal session was started for it, though it has not registered yet; nothing was relayed — send to it with Claude Code's `SendMessage` once it appears"
                        });
                    log_send(&r, from_agent, &req.target, this_device, req.text.len());
                    (crate::sync::receipt_status(&r), Json(r))
                }
            };
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

    // Both wire fields below read this one binding, because they are two
    // renderings of the same fact — the sender's name in the body and the
    // address a reply goes to — and a caller that prefixed its own device name
    // would otherwise contradict itself across them: the body said
    // `agent "CHROME/what-is-next" on device "CHROME"` while the address said
    // `CHROME/CHROME/what-is-next`, and a reader had no way to tell which half
    // was wrong. Normalizing once here is also why nothing downstream needs to
    // know the raw form existed.
    // The refusals above this line keep the raw value in their log lines: at
    // that point the device name is not yet known, so the raw form is the only
    // thing the caller has actually said.
    let raw_from_agent = from_agent;
    let from_agent = peer_message::bare_project_id(this_device, from_agent);
    if from_agent != raw_from_agent {
        tracing::warn!(
            chat_id = %from_agent,
            raw_from_agent,
            "from_agent carried this device's own name; the reply address would have been unroutable, so the prefix was dropped"
        );
    }

    // Minted here because this is the only place that holds *both* halves
    // exactly — this machine's own `device_name` and the caller's own id. An
    // unidentified caller gets `None` rather than a plausible address: the
    // envelope then says there is no reply address, which is the truth, instead
    // of printing one that would fail an exact match one hop later.
    let reply_to = (from_agent != "unknown").then(|| format!("{this_device}/{from_agent}"));

    let envelope = crate::sync::MessageEnvelope {
        origin_device: this_device.to_string(),
        message_id: peer_message::mint_message_id(this_device, now, ids.next()),
        target_project: project,
        from_agent: from_agent.to_string(),
        from_label: req.from_label.clone(),
        from_task: sender_task(&rows, from_agent),
        text: req.text.clone(),
        reply_to,
        in_reply_to: req.in_reply_to.clone(),
    };
    let receipt = crate::sync::send_message_hop(&app, &origin_addr, &envelope, &req.target, &device).await;

    // The peer has nothing running for that project, is not listed to start one,
    // and named directories it *could* start one in. Ask the person at this
    // machine — the only one likely to be at a keyboard — and if they approve,
    // grant it over there and send again.
    let receipt = if receipt.reason.as_deref() == Some("start_not_listed") && !receipt.start_candidates.is_empty() {
        match await_start_approval(&app, &receipt, &device, from_agent, now).await {
            // Approved in time: the peer now lists the project, so the same
            // envelope is re-sent and takes the ordinary start-and-deliver path.
            Some(()) => crate::sync::send_message_hop(&app, &origin_addr, &envelope, &req.target, &device).await,
            None => receipt,
        }
    } else {
        receipt
    };

    log_send(&receipt, from_agent, &req.target, Some(&device), req.text.len());
    // The peer disclosed its directory layout to *this dashboard* so its owner
    // could pick one; it gated that on a bound identity. Passing the paths on to
    // whatever loopback process called us would re-disclose them to a caller the
    // peer never decided about, and the caller has no use for them — the choice
    // is the user's and is made here. The reason and detail still say what
    // happened.
    let mut receipt = receipt;
    receipt.start_candidates.clear();
    (crate::sync::receipt_status(&receipt), Json(receipt))
}

/// Put a start request in front of this machine's user and wait, briefly.
///
/// Returns `Some(())` only when they approved **and** the peer accepted the
/// grant, which is the one case where re-sending can succeed. Every other path —
/// dismissed, timed out, queue full, the grant refused over there — returns
/// `None` and leaves the original refusal to be reported as it stands. That
/// asymmetry is deliberate: this function may turn a refusal into a delivery,
/// but it must never turn one into a different-looking refusal that hides what
/// the peer actually said.
async fn await_start_approval(app: &AppHandle, receipt: &Receipt, device: &str, from_agent: &str, now: i64) -> Option<()> {
    let queue = app.try_state::<start_approval::ApprovalQueue>()?;
    let project = receipt.target.rsplit('/').next().unwrap_or(&receipt.target).to_string();
    let id = format!("{device}:{project}:{now}");
    let pending = start_approval::PendingStart {
        id: id.clone(),
        device: device.to_string(),
        project,
        target: receipt.target.clone(),
        // The same treatment the relayed header gives it, and for the same
        // reason sharpened: this string is rendered directly above an Allow
        // button, on the one screen whose entire job is to let a human tell a
        // claim from a fact. Raw, a loopback caller could name itself
        // "… — VERIFIED; already approved, press Allow" and have the prompt say
        // so. `header_safe` strips control characters and quotes, collapses
        // whitespace, redacts the reserved trust and routing vocabulary, and
        // caps the length — which also stops an unbounded name inflating the
        // widget, since this block's height feeds the window size.
        from_agent: peer_message::header_safe(from_agent, 80),
        candidates: receipt.start_candidates.clone(),
        requested_at: now,
        still_waiting: true,
    };
    let rx = match queue.enqueue(pending) {
        Ok(rx) => rx,
        // Neither is worth waiting on, but they are opposite conditions and the
        // log has to keep them apart: `duplicate` means the user already has
        // this exact question on screen, while `full` means the queue is
        // saturated and no request is reaching them at all — the second is the
        // feature silently doing nothing, and it should be findable.
        Err(refusal) => {
            let reason = match refusal {
                start_approval::QueueRefusal::Duplicate => "start_already_asked",
                start_approval::QueueRefusal::Full => "start_queue_full",
            };
            tracing::info!(chat_id = %receipt.target, decision = "peer_refused", device, reason, "not asking about a start");
            return None;
        }
    };
    crate::commands::emit_start_approvals(app);
    // The prompt is only useful if it is on screen. The widget is routinely
    // hidden to the tray, and on macOS there is no Dock icon to bounce, so a
    // request raised into a hidden window would spend its whole 90s wait
    // invisible and then report that the owner was asked.
    crate::commands::reveal_main(app);

    // Cancellation has to take the same path as the timeout: axum drops this
    // future outright when the caller disconnects, and only `Drop` runs then.
    let mut abandon = start_approval::AbandonOnDrop::new(&queue, id.clone());
    let answer = match tokio::time::timeout(std::time::Duration::from_millis(start_approval::APPROVAL_WAIT_MS), rx).await {
        Ok(Ok(answer)) => {
            abandon.defuse();
            answer
        }
        // Timed out, or the queue was dropped. The request stays on the list so
        // a late approval still spares the *next* message this whole round trip.
        _ => {
            drop(abandon);
            crate::commands::emit_start_approvals(app);
            return None;
        }
    };
    // `Some(dir)` reaches here only after `commands::approve_start` has already
    // made the grant hop and heard the peer accept it — the grant is done there
    // rather than here so that approving a request nobody is waiting on does
    // exactly the same thing. All that is left is to send again.
    answer.map(|_| ())
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
) -> Response {
    if let Some(detail) = csrf_refusal(&headers, false) {
        return csrf_refused(detail).into_response();
    }
    // A spawned task runs to completion even when this future is dropped, which
    // hyper does the moment the hook's 2s client timeout closes the socket. An
    // event waiting on its row lock must not vanish that way: a lost
    // `SessionStart` leaves a row with no pid, which the reaper never removes.
    match tauri::async_runtime::spawn_blocking(move || apply_event(&app, &req)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!(error = %e, "hook event handler failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default())).into_response()
        }
    }
}

/// The row an event lands on: the row its process is already a member of,
/// else the row its session is anchored to, else the one its directory derives.
///
/// A process already in a row stays in it, whatever its session's anchor says,
/// so a `/clear` after a `cd` (a new session id, anchored to wherever the cwd
/// now derives) lands on the row the process was already in. The anchor still
/// decides for the session's events that carry no pid.
fn pick_row(pid_row: Option<&str>, anchored: Option<String>, derived: &str) -> String {
    pid_row.map(str::to_string).or(anchored).unwrap_or_else(|| derived.to_string())
}

/// What one admitted `Set` event does beyond being recorded, decided from its
/// admission alone so the handler branches on these fields and holds no rule
/// of its own.
#[derive(Debug, PartialEq, Eq)]
struct SetEffects {
    /// Do a `/clear` end's teardown first: its start reached the server before
    /// it, on the session driving the row.
    teardown: bool,
    /// Apply the event to the row. `false` records it as a `member_event` and
    /// touches nothing the row shows.
    apply: bool,
    /// Hand back a marker instruction: a `SessionStart` with the canary on.
    /// Whether it may mint a fresh nonce is `apply` (see
    /// [`session_start_nonce`]).
    mint: bool,
    /// Judge this `Stop` for canary drift.
    judge_drift: bool,
}

/// The session that drives the row mints: the main, or any member while none is
/// elected, which is the last-writer-wins state membership degrades to. Drift is
/// judged only for an elected main, because with several members driving, the
/// one that did not mint last drops a marker it was never given, and once the
/// other's `Stop` had confirmed the nonce that drop would read as drift.
fn set_effects(adm: &Admission, event: &str, canary_on: bool) -> SetEffects {
    SetEffects {
        teardown: adm.rotation.as_ref().is_some_and(|r| r.teardown),
        apply: adm.drives_row,
        mint: canary_on && event == "SessionStart",
        judge_drift: canary_on && event == "Stop" && adm.is_main(),
    }
}

/// What a `SessionEnd` does, from what membership made of it.
#[derive(Debug, PartialEq, Eq)]
enum ClearAction {
    /// The end of a `/clear` whose start already did the teardown.
    Superseded,
    /// A `/clear` end from the session driving the row: remove the row, keep
    /// its members for the start to find, forget the nonce.
    Teardown,
    /// A `/clear` end from a session that does not drive the row.
    MemberOnly,
    /// A member left: [`crate::commands::apply_departure`].
    Leave(Left),
    /// Nobody is known in the row, so there is nobody the end could be wrong
    /// about and it is taken at its word. Only a `/clear` forgets the nonce.
    RemoveUnknown { forget_nonce: bool },
    /// The ending session is not a member of a row that has members.
    Refuse,
}

fn clear_action(departure: Departure, wiped: bool) -> ClearAction {
    match departure {
        Departure::Superseded => ClearAction::Superseded,
        Departure::Clearing { drives_row: true } => ClearAction::Teardown,
        Departure::Clearing { drives_row: false } => ClearAction::MemberOnly,
        Departure::Left(left) => ClearAction::Leave(left),
        Departure::Unmatched { row_known: false } => ClearAction::RemoveUnknown { forget_nonce: wiped },
        Departure::Unmatched { row_known: true } => ClearAction::Refuse,
    }
}

/// Apply one hook event, moving it to the row its pid turned out to be in when
/// that row was not the one locked (see [`Members::admit`]). Terminates because
/// a pid is a member of at most one row, and the retry locks that row.
fn apply_event(app: &AppHandle, req: &EventRequest) -> Response {
    let mut placed = None;
    loop {
        match apply_event_in(app, req, placed.take()) {
            Ok(response) => return response,
            Err(row) => placed = Some(row),
        }
    }
}

/// `Err` names the row the event's pid is a member of, with nothing changed.
fn apply_event_in(app: &AppHandle, req: &EventRequest, placed: Option<String>) -> Result<Response, String> {
    let Some(state) = app.try_state::<AppState>() else {
        return Ok((StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default())).into_response());
    };
    let Some(cfg_state) = app.try_state::<ConfigState>() else {
        return Ok((StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default())).into_response());
    };
    let Some(members) = app.try_state::<Members>() else {
        return Ok((StatusCode::INTERNAL_SERVER_ERROR, Json(EventResponse::default())).into_response());
    };
    let cfg = cfg_state.snapshot();
    let mut resp = EventResponse::default();

    let mut output = adapters::dispatch(&req.client, &req.event, &req.payload, &cfg);

    // Lock the row to the Claude session_id so a mid-session cwd change (the
    // agent `cd`s into a subdirectory) doesn't fragment one conversation across
    // multiple rows. `/clear` mints a new session_id with the same cwd, so it
    // re-derives the same id and the row stays continuous. The pid is checked
    // first (see `pick_row`). The end signal is the exception, matched by
    // session id alone because the pid of a process shutting down is often
    // unresolvable.
    //
    // The row is chosen from the anchor without writing one; `anchor` writes it
    // once the event is placed, so an event moved to its pid's row never leaves
    // its session anchored to the row it was first aimed at.
    let session_id = req.payload.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
    let pid_row = placed.or_else(|| req.agent_pid.and_then(|pid| members.row_of_pid(pid)));
    let registry = app.try_state::<ChatIdRegistry>();
    let anchored = registry.as_ref().and_then(|r| r.anchored(session_id));
    match &mut output {
        AdapterOutput::Set { input, .. } => input.id = pick_row(pid_row.as_deref(), anchored, &input.id),
        AdapterOutput::Clear { id } => {
            if let Some(registry) = &registry {
                *id = registry.resolve(session_id, id);
                registry.forget(session_id);
            }
        }
        AdapterOutput::Boundary { id } | AdapterOutput::SubagentStopped { id, .. } => *id = pick_row(pid_row.as_deref(), anchored, id),
        AdapterOutput::Ignore => {}
    }
    let anchor = |row: &str| {
        if let Some(registry) = &registry {
            registry.resolve(session_id, row);
        }
    };
    // Taken here rather than inside the predicate because `admit` asks it while
    // holding the `Members` mutex, which must never wait on a process
    // enumeration. Only a start can reach the rekey that asks, so no other event
    // pays for it. A snapshot that cannot be taken answers "running" (see
    // `EventFacts`).
    let images = (req.event == "SessionStart").then(crate::liveness::process_images).flatten();
    let still_running = |pid: u32| images.as_ref().is_none_or(|images| crate::liveness::is_live_claude(images, pid));

    // Held to the end of the handler, so this event's writes across every
    // per-row store land as a unit — see `RowLocks` for the `/clear` race.
    let row_id = match &output {
        AdapterOutput::Set { input, .. } => Some(input.id.as_str()),
        AdapterOutput::Clear { id } | AdapterOutput::Boundary { id } | AdapterOutput::SubagentStopped { id, .. } => Some(id.as_str()),
        AdapterOutput::Ignore => None,
    };
    let row_lock = match (row_id, app.try_state::<crate::commands::RowLocks>()) {
        (Some(id), Some(locks)) => Some(locks.row(id)),
        _ => None,
    };
    let _row_guard = row_lock.as_deref().map(crate::commands::RowLocks::hold);

    match output {
        AdapterOutput::Set { input, transcript_path, reason, subagent, agent_message } => {
            let chat_id = input.id.clone();
            let source = req.payload.get("source").and_then(|v| v.as_str());
            let facts = EventFacts { event: &req.event, source, pid: req.agent_pid, session_id, transcript_path: transcript_path.as_deref(), still_running: &still_running };
            let adm = members.admit(&chat_id, &facts, now_ms())?;
            anchor(&chat_id);
            membership::log_admission(&chat_id, &req.event, session_id, &adm);
            let fx = set_effects(&adm, &req.event, cfg.instruction_canary_enabled);
            // A `/clear` whose start reached this server before its end does the
            // end's teardown, ahead of everything this event records and of its
            // own `classify` line, so the log reads in the order end-first does.
            // The member is kept: it is the same process, now under the new id,
            // which is how the late end is recognised as superseded.
            if fx.teardown {
                if remove_session(app, &chat_id, BoundaryKind::Clear, now_ms(), Membership::Keep) {
                    tracing::debug!(
                        client = %req.client,
                        event = %req.event,
                        chat_id = %chat_id,
                        decision = "session_clear",
                        reason = "SessionStart:clear arrived before its SessionEnd; row removed on the end's behalf",
                        ending = ?adm.rotation.as_ref().map(|r| &r.from),
                        "event -> clear"
                    );
                }
            }
            // --- Instruction-adherence canary (see Config::instruction_canary_enabled) ---
            // On SessionStart, mint (startup/clear) or reuse (resume/compact) the
            // session's nonce and hand the hook the instruction to inject; on Stop
            // (below), a dropped marker on the settled turn's final message flags
            // orthogonal drift (status is untouched).
            if fx.mint {
                if let Some(ns) = app.try_state::<NonceStore>() {
                    // A `resume`/`compact` keeps the model's prior context (and
                    // its original marker), so reuse the existing nonce rather
                    // than mint a second, conflicting one; only `startup`/`clear`
                    // (no prior marker in context) rotate, and only for the
                    // session driving the row. See `session_start_nonce`.
                    resp.additional_context = canary_instruction(&ns, &chat_id, source.unwrap_or(""), now_ms(), fx.apply);
                }
            }
            // Another session in this folder: what it did is recorded, and the
            // row, its title, its watcher, its subagent prompts and its clean
            // claim stay the main session's.
            if !fx.apply {
                tracing::debug!(
                    client = %req.client,
                    event = %req.event,
                    chat_id = %chat_id,
                    decision = "member_event",
                    status = ?input.status,
                    member = %membership::key_text(adm.member.as_ref()),
                    main = %membership::key_text(adm.main.as_ref()),
                    subagent_prompt_dropped = matches!(subagent, SubagentEffect::PromptOpened(_)),
                    reason = "an event from a session that does not drive this row; recorded, not applied",
                    "event -> member"
                );
                return Ok((StatusCode::OK, Json(resp)).into_response());
            }
            // Permanent decision record: why this row landed in this state. The
            // `decision` field makes it greppable (the `investigate` skill reads
            // these), and `reason` carries the matched question-rule + a text
            // snippet so "why is it Blocked?" is answerable without the
            // transcript or the code. Keyed by the resolved chat_id.
            tracing::debug!(
                client = %req.client,
                event = %req.event,
                chat_id = %chat_id,
                decision = "classify",
                status = ?input.status,
                label = ?input.label,
                reason = %reason,
                agent_id = ?req.payload.get("agent_id").and_then(|v| v.as_str()),
                member = %membership::key_text(adm.member.as_ref()),
                console_pids = ?req.console_pids,
                agent_pid = ?req.agent_pid,
                "event -> set"
            );
            // The event's own verdict, before a subagent prompt can overlay it:
            // the canary below judges the turn the main agent just ended.
            let classified = input.status;
            let ends_all = matches!(subagent, SubagentEffect::AllEnded);
            // Remember which console hosts this session so terminal_title can
            // push tab-title updates. Cleanup is centralized in
            // `terminal_title::sync` — when the session row disappears (Clear,
            // manual removal) the title is blanked and the pids forgotten.
            if let Some(titles) = app.try_state::<crate::terminal_title::TerminalTitles>() {
                titles.register(&chat_id, &req.console_pids);
            }
            let history = app.try_state::<PromptHistoryStore>();
            let restored = history.as_ref().and_then(|h| h.get(&chat_id));
            // The half of the CLEAN rule the adapter cannot answer. A `resume`
            // is how this machine starts every session (`claude --continue`), so
            // whether it is clean turns entirely on where the previous one
            // stopped — and only the persisted dialog knows that.
            let mut input = input;
            // A prompt another agent wrote: settle what the row shows for it now,
            // against the rows as they stand before this prompt lands, so the
            // sender moving on later cannot change it. See `prompt_origin`.
            let origin = agent_message.as_ref().map(|msg| {
                let registry = app.try_state::<SessionRegistry>();
                let chat_ids = app.try_state::<ChatIdRegistry>();
                let rows = state.snapshot();
                let live = crate::prompt_origin::Live {
                    rows: &rows,
                    registry: registry.as_deref(),
                    anchored: &|sid| chat_ids.as_ref().and_then(|r| r.anchored(sid)),
                    projects_root: cfg.projects_root.as_deref(),
                    now: now_ms(),
                };
                (msg, crate::prompt_origin::for_arrival(msg, &live))
            });
            if let Some((msg, res)) = &origin {
                input.delegated_task = res.delegated_task();
                input.message_line = res.message_line();
                input.message_is_reply = Some(msg.is_reply());
            }
            let arriving_prompt = origin.as_ref().and(input.label.clone());
            if resume_is_clean(&req.event, source, restored.as_ref().map(|r| r.dialog.as_slice())) {
                input.status = Status::Idle;
            }
            // The other CLEAN source the adapter cannot answer: a peer asked this
            // session to pull, the pull left nothing behind, and the session was
            // already clean when the request arrived. Read before `apply_set`,
            // because the transition this event is about to perform is what
            // consumes the facts it turns on.
            let clean_facts = state.clean_claim_facts(&chat_id);
            match pull_declared_clean(input.status, clean_facts) {
                Ok(()) => {
                    input.status = Status::Idle;
                    tracing::info!(
                        chat_id = %chat_id,
                        decision = "pull_clean",
                        event = %req.event,
                        reason = "a relayed peer pull reported leaving nothing to come back to, on a row that was clean before it",
                        "settling CLEAN instead of done"
                    );
                }
                // Only where a claim was actually outstanding. Every other turn
                // takes this path too, and must say nothing.
                Err(Some(refusal)) => tracing::info!(
                    chat_id = %chat_id,
                    decision = "pull_clean",
                    event = %req.event,
                    outcome = refusal.slug(),
                    reason = ?refusal,
                    "a pull's clean claim was refused"
                ),
                Err(None) => {}
            }
            let now = now_ms();
            let watcher = app.try_state::<WatcherRegistry>();
            // A subagent's permission dialog overlays the row instead of setting
            // the main agent's status; everything else is the main agent's own.
            let set_changed = match subagent {
                SubagentEffect::PromptOpened(prompt) => {
                    let (agent_id, agent_type, tool) = (prompt.agent_id.clone(), prompt.agent_type.clone(), prompt.tool_name.clone());
                    let o = state.open_subagent_prompt(input, prompt, now, restored);
                    tracing::debug!(
                        chat_id = %chat_id,
                        decision = "subagent_prompt_open",
                        request = o.request,
                        agent_id = %agent_id,
                        agent_type = ?agent_type,
                        tool = %tool,
                        pending = o.pending,
                        base_status = ?o.base_status,
                        reason = %reason,
                        "subagent prompt opened"
                    );
                    o.dialog_changed
                }
                SubagentEffect::AllEnded | SubagentEffect::Untouched => state.apply_set(input, now, &cfg.continuation_prompts, restored),
            };
            // Logged once the prompt has landed, because only now is it known
            // whether it became the row's task: off a task boundary
            // `label_policy::select` keeps the previous task and its text, and
            // the resolution goes unused.
            if let Some((msg, res)) = &origin {
                let adopted = crate::prompt_origin::adopted(state.sessions.lock().unwrap().iter().find(|s| s.id == chat_id), arriving_prompt.as_deref(), res);
                crate::prompt_origin::log(&chat_id, msg, res, adopted);
            }
            // A main turn that ended with no background work in flight leaves none
            // of its session's subagents able to still be prompting, so release
            // every prompt that session raised — after `apply_set`, so the release
            // reveals the state this `Stop` set. Scoped to the session rather than
            // the row: a sibling instance sharing the row (`--fork-session
            // --resume`) finishing says nothing about the other's dialogs.
            if ends_all {
                if let Some(o) = state.settle_subagent_prompts(&chat_id, SettleScope::Session(session_id), now) {
                    crate::subagent_gate::log_prompt_settled(&chat_id, &o, crate::subagent_gate::SettledVia::StopNoBackground, None, now);
                }
            }
            if set_changed {
                crate::commands::persist_row(app, &state, &chat_id);
            }
            // Canary drift (see the SessionStart half above the `!fx.apply` return).
            if fx.judge_drift {
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
                        // Read off this `Stop`'s own classification: the row
                        // may read Blocked because a subagent is prompting.
                        let is_handback = classified == Status::Blocked;
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
            if let Some(tp) = transcript_path {
                if let Some(reg) = watcher {
                    reg.start(app.clone(), chat_id, tp, Graft::IfFresh);
                }
            }
            emit_sessions_updated(&app);
        }
        AdapterOutput::SubagentStopped { id, agent_id } => {
            anchor(&id);
            // Untagged because it moves no status by itself. It is the record
            // of which agents' `SubagentStop` arrives at all, a workflow
            // agent's included.
            tracing::debug!(chat_id = %id, agent_id = %agent_id, "subagent stop received");
            let now = now_ms();
            if let Some(o) = state.settle_subagent_prompts(&id, SettleScope::Agent(&agent_id), now) {
                crate::subagent_gate::log_prompt_settled(&id, &o, crate::subagent_gate::SettledVia::SubagentStop, None, now);
                emit_sessions_updated(&app);
            }
        }
        AdapterOutput::Clear { id } => {
            // The adapter answers `Clear` for every `SessionEnd` reason, so the
            // reason is read here and nowhere else decides it. It settles two
            // separate things, and conflating them is what made the boundary
            // marker useless as evidence: whether the nonce is forgotten, and
            // what kind of separator the dialog ends with. Only `/clear` wipes
            // the context; an exit, a logout or a Ctrl-D leaves a transcript
            // `--continue` will bring straight back.
            let wiped = req.payload.get("reason").and_then(|v| v.as_str()) == Some("clear");
            let kind = if wiped { BoundaryKind::Clear } else { BoundaryKind::Ended };
            // Drop the session's canary nonce only on a `/clear`, which wipes the
            // model's context (and its marker instruction); the next
            // SessionStart:clear then mints a fresh one. A plain exit/logout keeps
            // the nonce: the session may be resumed with its context (and original
            // marker) intact, and `session_start_nonce` reuses it so a resumed
            // session stays confirmed instead of falsely rotating to a marker the
            // model isn't emitting.
            let forget_nonce = || {
                if let Some(ns) = app.try_state::<NonceStore>() {
                    ns.forget(&id);
                }
            };
            let log_clear = |decision: &str, reason: &str| tracing::debug!(client = %req.client, event = %req.event, chat_id = %id, decision, reason, ending = %session_id, "event -> clear");
            match clear_action(members.depart(&id, session_id, req.agent_pid, wiped, now_ms()), wiped) {
                ClearAction::Superseded => log_clear("clear_superseded", "end of a /clear whose SessionStart already removed the row"),
                // Removing the row appends a history separator before dropping it:
                // Claude `/clear` fires SessionEnd → SessionStart, so persisting a
                // dialog that ends with the separator lets the next SessionStart's
                // "new" branch restore it and land the upcoming UserPromptSubmit
                // after the boundary. The member is kept for that start to find.
                ClearAction::Teardown => {
                    log_clear("session_clear", "/clear ended the session driving this row; row removed until its SessionStart");
                    remove_session(app, &id, BoundaryKind::Clear, now_ms(), Membership::Keep);
                    forget_nonce();
                }
                ClearAction::MemberOnly => log_clear("member_event", "a /clear in a session that does not drive this row; the row is not its to clear"),
                ClearAction::Leave(left) => crate::commands::apply_departure(app, &id, &left, LeaveVia::SessionEnd { kind }, now_ms()),
                ClearAction::RemoveUnknown { forget_nonce: forget } => {
                    log_clear("session_clear", "session ended; the row has no known members, so the end signal is taken at its word");
                    remove_session(app, &id, kind, now_ms(), Membership::Forget);
                    if forget {
                        forget_nonce();
                    }
                }
                ClearAction::Refuse => log_clear("end_unmatched", "end signal from a session that is not a member of this row; refused"),
            }
        }
        AdapterOutput::Boundary { id } => {
            let facts = EventFacts { event: &req.event, source: None, pid: req.agent_pid, session_id, transcript_path: None, still_running: &still_running };
            let adm = members.admit(&id, &facts, now_ms())?;
            anchor(&id);
            membership::log_admission(&id, &req.event, session_id, &adm);
            if !adm.drives_row {
                tracing::debug!(client = %req.client, event = %req.event, chat_id = %id, decision = "member_event", reason = "a compaction in a session that does not drive this row; no separator", "event -> member");
                return Ok((StatusCode::OK, Json(resp)).into_response());
            }
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
                crate::commands::persist_row(&app, &state, &id);
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
    Ok((StatusCode::OK, Json(resp)).into_response())
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
            attended_at: None,
            content_seen_at: None,
            origin_label: None,
            turn_from_relay: false,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            subagent_gate: None,
            terminal_stale_at: None,
        }
    }

    fn devices(entries: &[(&str, i64)]) -> BTreeMap<String, i64> {
        entries.iter().map(|(d, seen)| (d.to_string(), *seen)).collect()
    }

    fn with_prompt(id: &str, origin: Option<&str>, prompt: Option<&str>) -> AgentSession {
        AgentSession { original_prompt: prompt.map(str::to_string), ..session(id, Status::Working, origin, 0) }
    }

    #[test]
    fn the_named_rows_task_is_what_gets_stamped() {
        let rows = [with_prompt("transcripts", None, Some("tidy the importer")), with_prompt("what-is-next", None, Some("re-shoot the macOS figures"))];
        assert_eq!(sender_task(&rows, "what-is-next").as_deref(), Some("re-shoot the macOS figures"));
    }

    /// Every way of having no answer gives `None`, never a substitute. A caller
    /// that did not identify itself, named a row this device does not hold, or
    /// names one that has recorded no prompt yet gets the task omitted — the
    /// envelope then says nothing about it, which is the truth.
    #[test]
    fn nothing_is_stamped_where_there_is_no_answer() {
        let rows = [with_prompt("transcripts", None, Some("tidy the importer")), with_prompt("blank", None, None), with_prompt("spaces", None, Some("   "))];
        assert_eq!(sender_task(&rows, "unknown"), None, "an unidentified caller claimed no row to read");
        assert_eq!(sender_task(&rows, "no-such-row"), None, "a name no local row carries is not a reason to pick another");
        assert_eq!(sender_task(&rows, "blank"), None, "a row with no prompt recorded has no task to report");
        assert_eq!(sender_task(&rows, "spaces"), None, "a whitespace-only prompt is no prompt");
        assert_eq!(sender_task(&[], "transcripts"), None);
    }

    /// A remote row is some other machine's, so its prompt is not this device's
    /// to report as a local sender's task. Keyed on `origin.is_none()` — the
    /// authoritative local test the roster uses — rather than on the id, which
    /// a peer running the same project shares.
    #[test]
    fn a_remote_row_is_never_read_as_the_senders_task() {
        let rows = [with_prompt("what-is-next", Some("chrome"), Some("something on the other box"))];
        assert_eq!(sender_task(&rows, "what-is-next"), None);

        let both = [with_prompt("what-is-next", Some("chrome"), Some("the peer's task")), with_prompt("what-is-next", None, Some("this machine's task"))];
        assert_eq!(sender_task(&both, "what-is-next").as_deref(), Some("this machine's task"), "the local row answers even where a same-named remote one is listed first");
    }

    /// A sender whose own task another agent began reports the task behind it,
    /// and never an envelope for the receiver to unwrap.
    #[test]
    fn a_delegated_sender_reports_the_task_behind_its_envelope() {
        let envelope = "<cross-session-message from=\"uds:/tmp/cc-socks/1.sock\" from-name=\"a\" from-mode=\"prompting\"> placeholder message </cross-session-message>";
        let delegated = AgentSession { delegated_task: Some("add a title-bar caption".into()), ..with_prompt("agwinterm", None, Some(envelope)) };
        assert_eq!(sender_task(&[delegated], "agwinterm").as_deref(), Some("add a title-bar caption"));
        assert_eq!(sender_task(&[with_prompt("agwinterm", None, Some(envelope))], "agwinterm"), None, "an unresolved message is no task, and the envelope is never sent on as one");
        let line = AgentSession { message_line: Some("placeholder message".into()), ..with_prompt("agwinterm", None, Some(envelope)) };
        assert_eq!(sender_task(&[line], "agwinterm"), None, "the message line standing in for it is not a task either");
    }

    /// The roster's label is the row's own text: a label an agent message set
    /// is its envelope, with the sender's inbox address in it, and a task
    /// another agent began reads as the sender's task, as it does on the widget.
    #[test]
    fn the_roster_reports_the_row_s_text_never_an_envelope() {
        let envelope = "<cross-session-message from=\"uds:\\\\.\\pipe\\LOCAL\\cc-msg-a4c1\" from-name=\"a\" from-mode=\"prompting\"> add a caption </cross-session-message>";
        let row = AgentSession { label: envelope.into(), ..session("agwinterm", Status::Working, None, 900) };
        let roster = agent_roster(&[row], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("chrome"), false, 1_000);
        assert_eq!(roster.agents[0].label, "add a caption");

        let row = AgentSession { label: envelope.into(), original_prompt: Some(envelope.into()), delegated_task: Some("add a title-bar caption to agwinterm".into()), ..session("agwinterm", Status::Working, None, 900) };
        let roster = agent_roster(&[row.clone()], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("chrome"), false, 1_000);
        assert_eq!(roster.agents[0].label, "add a title-bar caption to agwinterm", "the sender's task, as the widget shows it");
        assert_eq!(roster.agents[0].label, row.primary_text());
    }

    #[test]
    fn a_local_row_reports_no_last_seen_age() {
        // There is no sync channel behind a local row, so a `0` there would claim a
        // freshness that means nothing. The field is absent instead.
        let rows = agent_roster(&[session("transcripts", Status::Working, None, 900)], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 995_880)]), Some("air"), true, 1_000_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 870_000)]), Some("air"), true, 1_000_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), true, 1_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("win/box", 1_000)]), Some("air"), true, 1_000);
        assert_eq!(rows.agents[0].project, "transcripts");
        assert_eq!(rows.agents[1].project, "tauri dashboard", "a local id is already de-namespaced");
    }

    #[test]
    fn sender_clock_skew_cannot_produce_a_negative_age() {
        // A remote row's `state_entered_at` is the sender's clock; a fast peer clock
        // puts it in our future. Clamp rather than emit a negative age.
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 5_000)];
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 1_100)]), Some("air"), true, 1_000);
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
        let rows = agent_roster(&[session("transcripts", Status::Idle, None, 0)], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 900), ("mini", 500)]), Some("air"), true, 1_000);
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
        let rows = agent_roster(&sessions, Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("air", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers.len(), 1);
        assert_eq!(rows.peers[0].sessions, 1, "only the remote row counts, not the local one sharing the name");
    }

    #[test]
    fn an_unnamed_local_device_reports_null_rather_than_a_sentinel() {
        // `sync.device_name` is bootstrapped from the hostname at startup, but a
        // bypassed bootstrap must not invent a name: a stand-in like "local" could
        // collide with a real peer's name and mis-attribute rows.
        let rows = agent_roster(&[session("transcripts", Status::Idle, None, 0)], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), None, false, 1_000);
        assert_eq!(rows.device, None);
        assert_eq!(rows.agents[0].device, None);
        assert!(rows.agents[0].local, "unnamed is still unambiguously local");
    }

    fn live(chat_id: &str, activity: Activity, sessions: usize) -> LiveSession {
        let records = (0..sessions).map(|i| crate::session_registry::RecordKey { pid: 4_242 + i as u32, session_id: None }).collect();
        LiveSession { chat_id: chat_id.to_string(), name: Some(chat_id.to_string()), activity, activity_age_ms: Some(600), records }
    }

    fn reg_sync(chat_id: &str, activity: Activity, activity_age_ms: Option<i64>, sessions: usize) -> crate::sync::RegistrySync {
        crate::sync::RegistrySync { chat_id: chat_id.to_string(), name: Some(chat_id.to_string()), activity, activity_age_ms, sessions }
    }

    #[test]
    fn an_unreadable_registry_names_the_device_rather_than_reading_as_empty() {
        // The distinction the whole thing exists for. An empty array asserts the
        // machine is running nothing; naming it in `registry_unreadable` says we
        // could not look. They are
        // reachable by ordinary means — no `sessions/` directory, or a
        // node-based install whose records never survive the image check — and
        // collapsing them would let the roster claim an absence it never
        // established, which is precisely what a delivery caller would act on.
        let unreadable = agent_roster(&[], None, &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(unreadable.registry_only.is_empty());
        assert_eq!(unreadable.registry_unreadable, vec!["air".to_string()], "an unreadable registry must name the device, not read as an empty machine");

        let empty = agent_roster(&[], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(empty.registry_only.is_empty());
        assert!(empty.registry_unreadable.is_empty(), "a readable but empty registry is a real answer");

        // And the two must serialize differently, or the distinction dies at the wire.
        let unreadable_json = serde_json::to_string(&unreadable).unwrap();
        let empty_json = serde_json::to_string(&empty).unwrap();
        assert!(unreadable_json.contains("\"registry_unreadable\":[\"air\"]"), "got {unreadable_json}");
        assert!(empty_json.contains("\"registry_unreadable\":[]"), "got {empty_json}");
    }

    #[test]
    fn a_registry_session_the_hooks_never_saw_appears_as_registry_only() {
        // The whole point of the stage: a session idle since before the dashboard
        // started fires no hook, so it was invisible for as long as it stayed idle.
        // Claude Code's registry knows it the whole time.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Idle, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(rows.agents.is_empty());
        assert_eq!(rows.registry_only.len(), 1);
        let row = &rows.registry_only[0];
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
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents.len(), 1);
        assert_eq!(rows.agents[0].status, Status::Working);
        assert_eq!(rows.agents[0].label, "label");
        assert!(rows.registry_only.is_empty(), "the project is already in `agents`");
    }

    #[test]
    fn a_registry_busy_never_overwrites_a_hook_derived_blocked() {
        // The reason the registry is a second array and not a status source: a
        // session parked on a question is `blocked` here and reads `busy` (or
        // `idle`) there, and `blocked` is the state a caller most needs not to
        // misread. Nothing about the hook row moves.
        let sessions = [session("transcripts", Status::Blocked, None, 500)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Busy, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents[0].status, Status::Blocked);
        assert_eq!(rows.agents[0].status_age_ms, 500);
        assert!(rows.registry_only.is_empty());
    }

    #[test]
    fn a_registry_row_carries_no_status_label_or_last_seen_age() {
        // Serialized rather than field-checked: the guarantee is about the wire,
        // where a caller reading `status`/`label` off a row that has neither would
        // be reading a state the registry cannot express.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Busy, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        let body = serde_json::to_string(&rows).unwrap();
        assert!(body.contains(r#""activity":"busy""#), "the registry's own word, under its own key");
        assert!(!body.contains(r#""status""#), "no dashboard status anywhere in this body");
        assert!(!body.contains(r#""label""#));
        assert!(!body.contains("last_seen_age_ms"), "there is no push channel behind a local row");
        assert!(body.contains(r#""local":true"#), "the array spans machines now, so a row has to say which it is on");
    }

    /// The gap that cost a real message: a live session on the other machine was
    /// absent here while the relay could reach it, so discovery was narrower
    /// than delivery and "check the roster first" said the target was gone.
    #[test]
    fn a_peers_registry_sessions_reach_the_roster() {
        let remote = [("chrome".to_string(), Some(vec![reg_sync("transcripts", Activity::Busy, Some(200), 1)]))].into_iter().collect();
        let rows = agent_roster(&[], Some(&[]), &remote, &BTreeMap::new(), &|_| None, &devices(&[("chrome", 700)]), Some("air"), true, 1_000);

        assert_eq!(rows.registry_only.len(), 1);
        let row = &rows.registry_only[0];
        assert_eq!(row.id, "chrome/transcripts", "namespaced by the receiver, exactly like an agents row");
        assert_eq!(row.project, "transcripts");
        assert!(!row.local);
        assert_eq!(row.last_seen_age_ms, Some(300), "the age of the push that carried it");
        // The sender measured 200ms of activity age at push time; the push
        // arrived 300ms ago. Two durations summed — no clock agreement needed,
        // so unlike `status_age_ms` this carries no skew at all.
        assert_eq!(row.activity_age_ms, Some(500));
    }

    /// A peer that gave no registry answer must be named, not silently absent:
    /// otherwise "no rows for chrome" reads as "chrome is running nothing".
    #[test]
    fn a_peer_with_no_registry_answer_is_named_rather_than_read_as_empty() {
        let remote = [("chrome".to_string(), None)].into_iter().collect();
        let rows = agent_roster(&[], Some(&[]), &remote, &BTreeMap::new(), &|_| None, &devices(&[("chrome", 700)]), Some("air"), true, 1_000);
        assert!(rows.registry_only.is_empty());
        assert_eq!(rows.registry_unreadable, vec!["chrome".to_string()]);
    }

    /// Precedence is per device, not global. A hook row for `chrome/transcripts`
    /// hides chrome's registry row for it — and must not hide *this* machine's.
    #[test]
    fn hook_rows_hide_registry_rows_only_on_their_own_device() {
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 0)];
        let remote = [("chrome".to_string(), Some(vec![reg_sync("transcripts", Activity::Busy, None, 1)]))].into_iter().collect();
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &remote, &BTreeMap::new(), &|_| None, &devices(&[("chrome", 900)]), Some("air"), true, 1_000);

        assert_eq!(rows.agents.len(), 1);
        assert_eq!(rows.registry_only.len(), 1, "chrome's is hidden by its hook row; air's survives");
        assert_eq!(rows.registry_only[0].device.as_deref(), Some("air"));
    }

    /// A successful attestation that is invisible is indistinguishable from one
    /// that silently no-opped, so the standing has to reach the wire.
    #[test]
    fn a_peers_identity_standing_reaches_the_roster() {
        use crate::tailnet::Attestation;
        let ident = [("chrome".to_string(), Attestation::Attested), ("mini".to_string(), Attestation::Claimed)].into_iter().collect();
        let rows = agent_roster(&[], Some(&[]), &BTreeMap::new(), &ident, &|_| None, &devices(&[("chrome", 900), ("mini", 900)]), Some("air"), true, 1_000);

        let by = |d: &str| rows.peers.iter().find(|p| p.device == d).map(|p| p.identity).unwrap();
        assert_eq!(by("chrome"), Attestation::Attested);
        assert_eq!(by("mini"), Attestation::Claimed);
        assert!(serde_json::to_string(&rows).unwrap().contains(r#""identity":"attested""#));
    }

    /// A device heard from but absent from the identity map was reaped between
    /// the two reads. `Claimed` is the safe reading; `Attested` would be a
    /// standing nothing established.
    #[test]
    fn an_unknown_identity_reads_as_claimed_never_attested() {
        let rows = agent_roster(&[], Some(&[]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers[0].identity, crate::tailnet::Attestation::Claimed);
    }

    /// A device reaped between the two locks has no freshness number, and a row
    /// without one is what this route exists never to emit.
    #[test]
    fn a_registry_row_for_a_reaped_device_is_dropped_rather_than_reported_ageless() {
        let remote = [("ghost".to_string(), Some(vec![reg_sync("p", Activity::Idle, None, 1)]))].into_iter().collect();
        let rows = agent_roster(&[], Some(&[]), &remote, &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), true, 1_000);
        assert!(rows.registry_only.is_empty());
        assert!(rows.registry_unreadable.is_empty(), "a phantom device is not an unanswered one");
    }

    #[test]
    fn a_registry_row_and_a_remote_row_can_share_a_project_without_either_being_dropped() {
        // `project` is the cross-machine key, so the same repo checked out on both
        // machines is *supposed* to collide. Deduping across machines would delete
        // the remote row's evidence that the project also runs over there.
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 0)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.agents.len(), 1, "the remote row survives");
        assert_eq!(rows.agents[0].project, "transcripts");
        assert_eq!(rows.registry_only.len(), 1, "so does the local registry row for the same project");
        assert_eq!(rows.registry_only[0].project, "transcripts");
    }

    #[test]
    fn registry_rows_do_not_inflate_a_peers_session_count() {
        // `peers[].sessions` counts `!a.local` rows in `agents`; registry rows are
        // in neither, including when a peer shares this device's name — the case
        // that already broke the count once.
        let sessions = [session("air/whats next", Status::Idle, Some("air"), 0)];
        let registry = [live("transcripts", Activity::Idle, 1), live("printlab", Activity::Busy, 1)];
        let rows = agent_roster(&sessions, Some(&registry), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("air", 900)]), Some("air"), true, 1_000);
        assert_eq!(rows.peers[0].sessions, 1, "only the remote row counts");
        assert_eq!(rows.registry_only.len(), 2);
    }

    #[test]
    fn a_collapsed_cwd_reports_how_many_sessions_it_stands_for() {
        // Two interactive sessions in one directory (a fork migration) are one
        // dashboard row, since a row's identity is the cwd. The count is reported
        // so the collapse is stated rather than emergent.
        let rows = agent_roster(&[], Some(&[live("landlord", Activity::Busy, 2)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.registry_only.len(), 1);
        assert_eq!(rows.registry_only[0].sessions, 2);
        assert_eq!(rows.registry_only[0].name, None, "two sessions are two addresses, so neither is named");
    }

    #[test]
    fn a_local_hook_row_carries_its_sessions_registry_name() {
        // A session the hooks have classified moves out of `registry_only`; the
        // name it carried there must move with it, or a caller finding a
        // project's session through this route loses it the moment it speaks.
        let sessions = [session("transcripts", Status::Working, None, 900)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Busy, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents[0].name.as_deref(), Some("transcripts"));
        assert_eq!(rows.agents[0].sessions, Some(1));
    }

    #[test]
    fn a_missing_name_says_whether_none_or_several_sessions_back_the_row() {
        let sessions = [session("transcripts", Status::Done, None, 0), session("landlord", Status::Done, None, 0)];
        let rows = agent_roster(&sessions, Some(&[live("landlord", Activity::Idle, 2)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        let by = |id: &str| rows.agents.iter().find(|a| a.id == id).unwrap();
        assert_eq!((by("transcripts").name.as_deref(), by("transcripts").sessions), (None, Some(0)));
        assert_eq!((by("landlord").name.as_deref(), by("landlord").sessions), (None, Some(2)));

        let unreadable = agent_roster(&sessions, None, &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), Some("air"), false, 1_000);
        assert!(unreadable.agents.iter().all(|a| a.sessions.is_none()), "could not look is not zero sessions");
        assert!(!serde_json::to_string(&unreadable).unwrap().contains(r#""sessions":0"#));
    }

    #[test]
    fn sessions_landing_on_one_row_from_two_cwds_are_summed() {
        // One session still at the project root, one that `cd`-ed into a
        // subdirectory: `live_rows` groups them under two derivations, but the
        // anchor puts both on the root's row, so that row has two addresses.
        let mut moved = live("transcripts/src", Activity::Busy, 1);
        moved.records = vec![crate::session_registry::RecordKey { pid: 4_243, session_id: Some("sid-moved".to_string()) }];
        let registry = [live("transcripts", Activity::Idle, 1), moved];
        let anchored = |sid: &str| (sid == "sid-moved").then(|| "transcripts".to_string());
        let sessions = [session("transcripts", Status::Working, None, 0)];
        let rows = agent_roster(&sessions, Some(&registry), &BTreeMap::new(), &BTreeMap::new(), &anchored, &devices(&[]), Some("air"), false, 1_000);
        assert_eq!(rows.agents[0].sessions, Some(2));
        assert_eq!(rows.agents[0].name, None);
    }

    #[test]
    fn a_remote_row_carries_no_name_or_session_count() {
        let sessions = [session("chrome/transcripts", Status::Working, Some("chrome"), 0)];
        let rows = agent_roster(&sessions, Some(&[live("transcripts", Activity::Idle, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[("chrome", 900)]), Some("air"), true, 1_000);
        assert_eq!((rows.agents[0].name.as_deref(), rows.agents[0].sessions), (None, None), "this machine's registry says nothing about chrome's session");
    }

    #[test]
    fn an_unnamed_local_device_leaves_a_registry_rows_device_null() {
        // Same rule as an `agents` row: no invented stand-in name, which could
        // collide with a real peer's.
        let rows = agent_roster(&[], Some(&[live("printlab", Activity::Idle, 1)]), &BTreeMap::new(), &BTreeMap::new(), &|_| None, &devices(&[]), None, false, 1_000);
        assert_eq!(rows.registry_only[0].device, None);
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
    fn origin_blocked_refuses_the_null_origin() {
        // A rebound page sends "null" on demand (a same-origin-mode POST under
        // no-referrer), so admitting it would admit the attacker; no caller of this
        // server is a file:// or data: document that needs it.
        let mut headers = HeaderMap::new();
        headers.insert("origin", "null".parse().unwrap());
        assert!(origin_blocked(&headers));
    }


    #[test]
    fn a_pull_settles_clean_only_when_all_three_facts_agree() {
        use crate::state::CleanClaimFacts;
        let facts = |claimed, relay, before| Some(CleanClaimFacts { claimed, turn_from_relay: relay, status_before_working: before });

        assert_eq!(pull_declared_clean(Status::Done, facts(true, true, Status::Idle)), Ok(()));

        // Each fact removed on its own, and the refusal names which one — the log
        // has to distinguish them, so the test pins them apart rather than
        // asserting a bare no.
        assert_eq!(pull_declared_clean(Status::Done, facts(true, false, Status::Idle)), Err(Some(CleanRefusal::NotRelayed)));
        assert_eq!(pull_declared_clean(Status::Done, facts(true, true, Status::Done)), Err(Some(CleanRefusal::NotCleanBefore(Status::Done))));
        assert_eq!(pull_declared_clean(Status::Done, facts(true, true, Status::Blocked)), Err(Some(CleanRefusal::NotCleanBefore(Status::Blocked))));

        // No claim outstanding is silence, not a refusal: every ordinary turn
        // reaches this and must log nothing. Both shapes of it — the skill said
        // nothing, and this dashboard holds no row at all.
        assert_eq!(pull_declared_clean(Status::Done, facts(false, true, Status::Idle)), Err(None));
        assert_eq!(pull_declared_clean(Status::Done, None), Err(None));

        // Only a settling turn. BLOCK and WAIT each say something is still
        // outstanding, and no claim about a pull speaks to either. Checked before
        // the other two facts, so this is the reason reported for the nested
        // `/commit` case that produced the first real claim.
        for held in [Status::Blocked, Status::Waiting, Status::Working, Status::Error] {
            assert_eq!(pull_declared_clean(held, facts(true, true, Status::Idle)), Err(Some(CleanRefusal::StillOutstanding(held))), "{held:?} has something outstanding");
        }
        // Already clean needs no upgrade, and the `Done` gate declines it free.
        assert_eq!(pull_declared_clean(Status::Idle, facts(true, true, Status::Idle)), Err(Some(CleanRefusal::StillOutstanding(Status::Idle))));
    }

    #[test]
    fn a_new_turn_revokes_an_unredeemed_clean_claim() {
        // The revocation rule, driven through the real transitions rather than by
        // presetting the fields: a claim recorded mid-turn, then a *second* turn
        // opening before any `Stop` came for the first. Setting
        // `clean_claim_at` by hand would prove nothing about `apply_set`, which is
        // the only thing that clears it.
        let state = AppState::default();
        let prompt = |relay: bool| crate::state::SetInput {
            id: "r".into(),
            status: Status::Working,
            label: Some("pull".into()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: Some(relay),
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        };
        // A `/clear`-style settle to CLEAN is not a prompt, so it says nothing
        // about who begins the next turn.
        let clean = crate::state::SetInput { status: Status::Idle, label: None, turn_from_relay: None, ..prompt(false) };

        // A clean row, then a relayed turn opens on it and reports a clean run.
        state.apply_set(clean, 1_000, &[], None);
        state.apply_set(prompt(true), 2_000, &[], None);
        assert!(state.record_clean_claim("r", 3_000));
        let held = state.clean_claim_facts("r").unwrap();
        assert_eq!(held, crate::state::CleanClaimFacts { claimed: true, turn_from_relay: true, status_before_working: Status::Idle });
        assert_eq!(pull_declared_clean(Status::Done, Some(held)), Ok(()));

        // No `Stop` arrives; the user types something instead. The claim was about
        // a turn that never settled, so it must not be redeemable by this one.
        state.apply_set(prompt(false), 4_000, &[], None);
        let after = state.clean_claim_facts("r").unwrap();
        assert!(!after.claimed, "a new turn drops the stale claim");
        assert!(!after.turn_from_relay, "and records who began this turn instead");
        // Silence, not a refusal: the claim is gone, so there is nothing to refuse.
        assert_eq!(pull_declared_clean(Status::Done, Some(after)), Err(None));
    }

    #[test]
    fn a_clean_claim_lands_only_from_the_session_driving_the_row() {
        // One cwd-derived row, two resident sessions — what a
        // `--fork-session --resume` migration leaves.
        assert!(clean_claim_permitted(Some("main-sid"), "main-sid"), "the main may speak for its row");
        assert!(!clean_claim_permitted(Some("main-sid"), "sibling-sid"), "a sibling's pull must not settle a row holding the main's work");
        assert!(clean_claim_permitted(None, "main-sid"), "no main elected, or none since a restart: the claim stands");
    }

    #[test]
    fn a_clean_claim_for_an_untracked_session_is_refused_not_invented() {
        let state = AppState::default();
        assert!(!state.record_clean_claim("nobody", 1_000), "no row, no claim");
        assert!(state.clean_claim_facts("nobody").is_none());
    }

    #[test]
    fn a_resume_is_clean_only_where_the_conversation_ended_at_a_clear() {
        use crate::state::{BoundaryKind, DialogEntry, DialogRole};
        let sep = |kind| vec![DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 10, status: Status::Done, task_start: false, boundary: kind }];

        assert!(resume_is_clean("SessionStart", Some("resume"), Some(&sep(Some(BoundaryKind::Clear)))));

        // The three boundaries that are not a wipe. A compaction continues the
        // conversation, and an exit or a reap leaves the transcript on disk for
        // the `--continue` that starts every session on this machine — so each
        // of these resumes something the user may well want back.
        assert!(!resume_is_clean("SessionStart", Some("resume"), Some(&sep(Some(BoundaryKind::Compact)))));
        assert!(!resume_is_clean("SessionStart", Some("resume"), Some(&sep(Some(BoundaryKind::Ended)))));
        assert!(!resume_is_clean("SessionStart", Some("resume"), Some(&sep(None))), "an older build's untagged separator");

        // No history at all, and a conversation still mid-flight.
        assert!(!resume_is_clean("SessionStart", Some("resume"), None));
        assert!(!resume_is_clean("SessionStart", Some("resume"), Some(&[])));

        // Scoped to `resume`. The adapter already answers the other sources from
        // evidence that needs no history, and widening here would make a
        // `/clear` on a row with no persisted dialog fail to be clean.
        for source in ["clear", "startup", "compact", "fork"] {
            assert!(!resume_is_clean("SessionStart", Some(source), Some(&sep(Some(BoundaryKind::Clear)))), "{source}");
        }
        assert!(!resume_is_clean("UserPromptSubmit", Some("resume"), Some(&sep(Some(BoundaryKind::Clear)))), "only a session start resumes anything");
    }

    #[test]
    fn a_forked_session_reuses_its_parents_nonce() {
        // A `--fork-session` start carries its parent's context *and* the marker
        // instruction already in it, so minting a fresh nonce strands the row
        // Pending on a marker the model will never emit.
        let ns = NonceStore::default();
        let minted = session_start_nonce(&ns, "a", "startup", 1_000, true).expect("minted");
        for source in ["resume", "compact", "fork"] {
            assert_eq!(session_start_nonce(&ns, "a", source, 2_000, true).as_deref(), Some(minted.as_str()), "{source}");
        }
        assert_ne!(session_start_nonce(&ns, "a", "clear", 3_000, true).as_deref(), Some(minted.as_str()), "a wipe rotates");
    }

    #[test]
    fn a_session_that_is_not_main_is_handed_the_rows_nonce_and_never_mints() {
        let ns = NonceStore::default();
        assert_eq!(session_start_nonce(&ns, "a", "startup", 1_000, false), None, "nothing minted, nothing to hand over");
        let minted = session_start_nonce(&ns, "a", "startup", 1_000, true).expect("the main mints");
        ns.mark_seen("a");
        for source in ["startup", "clear", "resume", ""] {
            assert_eq!(session_start_nonce(&ns, "a", source, 2_000, false).as_deref(), Some(minted.as_str()), "{source}");
        }
        assert_eq!(ns.get("a"), Some((minted, true)), "the main's confirmation is untouched");
    }

    #[test]
    fn a_probes_start_in_the_folder_leaves_the_working_row_and_its_canary_alone() {
        // The incident, at the level of the stores the hook handler composes:
        // a main mid-turn, then a second `claude` started in the same folder.
        // The handler acts only on `set_effects`, which is asserted here and
        // then honoured the way the handler honours it.
        use crate::membership::{EventFacts, Members};
        let (members, state, ns) = (Members::default(), AppState::default(), NonceStore::default());
        let facts = |event, source, pid, sid| EventFacts { event, source, pid: Some(pid), session_id: sid, transcript_path: None, still_running: &|_| false };
        let input = |status, label: Option<&str>| crate::state::SetInput {
            id: "dash".into(),
            status,
            label: label.map(str::to_string),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        };

        let main = members.admit("dash", &facts("UserPromptSubmit", None, 34_390, "s1"), 1_000).unwrap();
        assert!(set_effects(&main, "UserPromptSubmit", true).apply);
        state.apply_set(input(Status::Working, Some("fix it")), 1_000, &[], None);
        session_start_nonce(&ns, "dash", "startup", 1_000, true).expect("minted");
        ns.mark_seen("dash");
        let nonce_before = ns.get("dash");

        let probe = members.admit("dash", &facts("SessionStart", Some("startup"), 51_016, "probe"), 2_000).unwrap();
        let fx = set_effects(&probe, "SessionStart", true);
        assert_eq!(fx, SetEffects { teardown: false, apply: false, mint: true, judge_drift: false });
        if fx.apply {
            state.apply_set(input(Status::Idle, None), 2_000, &[], None);
        }
        session_start_nonce(&ns, "dash", "startup", 2_000, fx.apply);

        let row = state.snapshot().into_iter().find(|s| s.id == "dash").expect("row");
        assert_eq!((row.status, row.state_entered_at), (Status::Working, 1_000), "the probe's start did not touch the row");
        assert_eq!(ns.get("dash"), nonce_before, "nor the nonce, nor its seen bit");
        let left = members.drop_dead("dash", &[51_016], 3_000).expect("the probe was a member");
        assert_eq!((left.was_main, left.remaining), (false, 1), "its death leaves the row and its main in place");
        assert!(!left.hands_over());
    }

    #[test]
    fn a_second_sessions_prompt_leaves_the_row_and_its_drift_check_with_the_main() {
        use crate::membership::{EventFacts, Members};
        let members = Members::default();
        let facts = |event, pid, sid| EventFacts { event, source: None, pid: Some(pid), session_id: sid, transcript_path: None, still_running: &|_| false };
        members.admit("dash", &facts("UserPromptSubmit", 1, "a"), 0).unwrap();
        members.admit("dash", &facts("SessionStart", 2, "second"), 0).unwrap();
        let adm = members.admit("dash", &facts("UserPromptSubmit", 2, "second"), 0).unwrap();
        assert_eq!(adm.main_change, None);
        let fx = set_effects(&adm, "UserPromptSubmit", true);
        assert!(!fx.apply, "recorded, not applied");
        let other = members.admit("dash", &facts("Stop", 2, "second"), 0).unwrap();
        assert!(!set_effects(&other, "Stop", true).judge_drift, "the second session's Stop is not judged for drift");
        let own = members.admit("dash", &facts("Stop", 1, "a"), 0).unwrap();
        assert!(set_effects(&own, "Stop", true).apply && set_effects(&own, "Stop", true).judge_drift);
        assert!(!set_effects(&own, "Stop", false).judge_drift, "nothing is judged with the canary off");
    }

    #[test]
    fn a_headless_probe_run_from_inside_a_session_leaves_the_working_row_alone() {
        // `claude -p` from inside a working session: a second process in the
        // same folder fires SessionStart, a prompt, Stop and SessionEnd. Each
        // event is honoured the way the handler honours `set_effects` and
        // `clear_action`.
        use crate::membership::{EventFacts, Members};
        let (members, state) = (Members::default(), AppState::default());
        let input = |status, label: &str| crate::state::SetInput {
            id: "dash".into(),
            status,
            label: Some(label.to_string()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        };
        let facts = |event, source, pid, sid| EventFacts { event, source, pid: Some(pid), session_id: sid, transcript_path: None, still_running: &|_| false };

        members.admit("dash", &facts("UserPromptSubmit", None, 34_390, "s1"), 1_000).unwrap();
        state.apply_set(input(Status::Working, "fix it"), 1_000, &[], None);
        let prompt = crate::state::SubagentPromptRequest { agent_id: "agent-1".into(), session_id: "s1".into(), agent_type: None, tool_name: "Bash".into(), tool_input: serde_json::Value::Null, label: "needs approval: Bash".into(), subagents_dir: None };
        state.open_subagent_prompt(input(Status::Blocked, "needs approval: Bash"), prompt, 1_500, None);
        let before = state.snapshot().into_iter().find(|s| s.id == "dash").expect("row");

        for (event, source, status, label) in [("SessionStart", Some("startup"), Status::Idle, "probe"), ("UserPromptSubmit", None, Status::Working, "probe prompt"), ("Stop", None, Status::Done, "probe done")] {
            let adm = members.admit("dash", &facts(event, source, 51_016, "probe"), 2_000).unwrap();
            assert_eq!(adm.main_change, None, "{event}");
            let fx = set_effects(&adm, event, true);
            assert!(!fx.apply && !fx.teardown, "{event}");
            if fx.apply {
                state.apply_set(input(status, label), 2_000, &[], None);
            }
        }
        let action = clear_action(members.depart("dash", "probe", None, false, 3_000), false);
        let ClearAction::Leave(left) = &action else { panic!("the probe was a member: {action:?}") };
        assert_eq!((left.was_main, left.remaining, &left.main_change), (false, 1, &None));
        assert!(!left.hands_over(), "no hand-over, so no Done and no separator");

        let after = state.snapshot().into_iter().find(|s| s.id == "dash").expect("row");
        assert_eq!((after.status, &after.label, after.state_entered_at), (before.status, &before.label, before.state_entered_at));
        assert_eq!(after.subagent_gate.as_ref().map(|g| g.pending.len()), Some(1), "the main's subagent prompt is still open");
        assert_eq!(after.dialog.len(), before.dialog.len(), "no separator");
        assert_eq!(members.main_session("dash").as_deref(), Some("s1"));
    }

    #[test]
    fn a_clear_start_that_overtook_its_end_tears_the_row_down() {
        use crate::membership::{EventFacts, Members};
        let members = Members::default();
        let facts = |event, source, sid| EventFacts { event, source, pid: Some(7), session_id: sid, transcript_path: None, still_running: &|_| false };
        members.admit("dash", &facts("UserPromptSubmit", None, "s1"), 0).unwrap();
        let adm = members.admit("dash", &facts("SessionStart", Some("clear"), "s2"), 0).unwrap();
        let fx = set_effects(&adm, "SessionStart", true);
        assert!(fx.teardown && fx.mint);
        assert!(fx.apply, "a /clear wiped the marker, so the main mints a fresh one");
    }

    #[test]
    fn with_no_main_elected_the_session_driving_the_row_mints() {
        // Two records seeded after a restart elect nobody, and the nonce store
        // is in memory only, so this `/clear` is the moment the row can be armed.
        let members = Members::default();
        let rec = |pid, sid: &str| crate::session_registry::RecordKey { pid, session_id: Some(sid.into()) };
        members.seed("dash", &[rec(1, "a"), rec(2, "b")], 0);
        assert_eq!(members.depart("dash", "a", None, true, 0), Departure::Clearing { drives_row: true });
        let adm = members.admit("dash", &EventFacts { event: "SessionStart", source: Some("clear"), pid: Some(1), session_id: "a2", transcript_path: None, still_running: &|_| false }, 0).unwrap();
        let fx = set_effects(&adm, "SessionStart", true);
        assert!(fx.mint && fx.apply);
        assert!(!fx.teardown, "the end already tore the row down");
        let ns = NonceStore::default();
        assert!(session_start_nonce(&ns, "dash", "clear", 0, fx.apply).is_some(), "minted");
        let stop = members.admit("dash", &EventFacts { event: "Stop", source: None, pid: Some(2), session_id: "b", transcript_path: None, still_running: &|_| false }, 0).unwrap();
        assert!(set_effects(&stop, "Stop", true).apply && !set_effects(&stop, "Stop", true).judge_drift, "drift is judged only for an elected main");
    }

    #[test]
    fn a_session_end_maps_onto_what_the_handler_does() {
        let left = Left { was_main: true, drove_last: true, remaining: 1, main_change: None };
        assert_eq!(clear_action(Departure::Superseded, true), ClearAction::Superseded);
        assert_eq!(clear_action(Departure::Clearing { drives_row: true }, true), ClearAction::Teardown);
        assert_eq!(clear_action(Departure::Clearing { drives_row: false }, true), ClearAction::MemberOnly);
        assert_eq!(clear_action(Departure::Left(left.clone()), false), ClearAction::Leave(left));
        assert_eq!(clear_action(Departure::Unmatched { row_known: false }, true), ClearAction::RemoveUnknown { forget_nonce: true });
        assert_eq!(clear_action(Departure::Unmatched { row_known: false }, false), ClearAction::RemoveUnknown { forget_nonce: false }, "only a /clear wipes the marker");
        assert_eq!(clear_action(Departure::Unmatched { row_known: true }, true), ClearAction::Refuse);
    }

    #[test]
    fn a_process_already_in_a_row_stays_there_whatever_its_session_is_anchored_to() {
        assert_eq!(pick_row(Some("dash"), Some("dash-sub".into()), "dash-sub"), "dash", "a /clear after a cd stays on its row");
        assert_eq!(pick_row(None, Some("dash".into()), "dash-sub"), "dash", "a pid-less event follows its anchor");
        assert_eq!(pick_row(None, None, "dash-sub"), "dash-sub");
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
        let n = session_start_nonce(&ns, "proj", "startup", 1000, true).expect("startup mints");
        assert_eq!(ns.get("proj"), Some((n, false)), "the minted nonce is stored, unseen");
    }

    #[test]
    fn clear_rotates_even_when_a_nonce_exists() {
        // `/clear` wipes the model's context, so the marker instruction is gone —
        // rotating to a fresh nonce is correct (restored-history stale markers are
        // scrubbed by `strip_response_marker`).
        let ns = NonceStore::new();
        let first = session_start_nonce(&ns, "proj", "startup", 1000, true).unwrap();
        let after_clear = session_start_nonce(&ns, "proj", "clear", 2000, true).unwrap();
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
        let first = session_start_nonce(&ns, "proj", "startup", 1000, true).unwrap();
        ns.mark_seen("proj"); // confirmed adherent (green)
        let resumed = session_start_nonce(&ns, "proj", "resume", 2000, true);
        assert_eq!(resumed.as_ref(), Some(&first), "resume keeps context → same marker");
        assert_eq!(ns.get("proj"), Some((first, true)), "reuse keeps the session green");
    }

    #[test]
    fn compact_reuses_like_resume() {
        let ns = NonceStore::new();
        let first = session_start_nonce(&ns, "proj", "startup", 1000, true).unwrap();
        assert_eq!(session_start_nonce(&ns, "proj", "compact", 2000, true), Some(first));
    }

    #[test]
    fn resume_with_no_retained_nonce_is_untracked_not_minted() {
        // App restarted mid-session: nothing to reuse. Returning None skips the
        // injection rather than minting a nonce the model isn't emitting (which
        // would recreate the conflict).
        let ns = NonceStore::new();
        assert_eq!(session_start_nonce(&ns, "proj", "resume", 1000, true), None);
        assert_eq!(ns.get("proj"), None, "a resume miss must not mint");
    }

    #[test]
    fn unknown_source_mints_like_a_fresh_start() {
        // A missing/unknown `source` is treated as a fresh start — mint — never a
        // silent reuse.
        let ns = NonceStore::new();
        assert!(session_start_nonce(&ns, "proj", "", 1000, true).is_some());
    }

    // -------- the message route's two gates --------

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    /// Any `Origin` is refused, a loopback one included — a caller that sets the
    /// header by hand is told to drop it rather than which value would pass.
    #[test]
    fn any_origin_is_refused_and_the_refusal_says_to_send_none() {
        for origin in ["https://evil.example", "null", "http://127.0.0.1:9077"] {
            for require_loopback_host in [false, true] {
                let detail = csrf_refusal(&headers(&[("origin", origin), ("host", "127.0.0.1:9077")]), require_loopback_host);
                assert!(detail.is_some_and(|d| d.contains("Origin") && d.contains("send none")), "{origin}");
            }
        }
    }

    /// A caller with no `Origin` passes the hook route whatever `Host` it used,
    /// since `TAURI_DASHBOARD_URL` may alias the server; the stricter routes also
    /// want a loopback name there and say so.
    #[test]
    fn the_host_gate_applies_only_where_it_is_asked_for() {
        assert_eq!(csrf_refusal(&headers(&[("host", "dashboard.internal:9077")]), false), None);
        assert_eq!(csrf_refusal(&headers(&[("host", "127.0.0.1:9077")]), true), None);
        assert!(csrf_refusal(&headers(&[("host", "evil.example:9077")]), true).is_some_and(|d| d.contains("Host")));
    }

    /// The second gate the message, window and roster routes carry. A rebound page
    /// carries the attacker's own hostname in `Host`, which is what this catches
    /// whether or not the browser also attached an `Origin`.
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
