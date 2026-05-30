use crate::config::{Config, ConfigState};
use crate::custom_names::CustomNamesStore;
use crate::log_watcher::WatcherRegistry;
use crate::prompt_history::PromptHistoryStore;
use crate::setup;
use crate::state::{AgentSession, AppState};
use crate::telegram::TelegramNotifier;
use crate::usage_limits::{UsageLimits, UsageLimitsState};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

/// Snapshot the sessions and fill each `display_name` from the custom-names
/// store. The name rides on the session so the frontend renders it without a
/// separate lookup channel.
fn resolved_snapshot(app: &AppHandle) -> Vec<AgentSession> {
    let Some(state) = app.try_state::<AppState>() else {
        return Vec::new();
    };
    let mut sessions = state.snapshot();
    if let Some(names) = app.try_state::<CustomNamesStore>() {
        for s in &mut sessions {
            s.display_name = names.get(&s.id);
        }
    }
    sessions
}

#[tauri::command]
pub fn get_sessions(app: AppHandle) -> Vec<AgentSession> {
    resolved_snapshot(&app)
}

#[tauri::command]
pub fn get_config(app: AppHandle) -> Config {
    app.try_state::<ConfigState>().map(|s| s.snapshot()).unwrap_or_default()
}

#[tauri::command]
pub fn get_usage_limits(state: State<UsageLimitsState>) -> UsageLimits {
    state.snapshot()
}

#[tauri::command]
pub fn refresh_usage_limits(state: State<UsageLimitsState>) -> bool {
    state.request_refresh()
}

#[tauri::command]
pub fn apply_auto_resize(height: f64, app: AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let mode = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().auto_resize)
        .unwrap_or_default();
    if let Err(e) = crate::auto_resize::apply(&window, mode, height) {
        tracing::warn!(?e, height, "apply_auto_resize failed");
    }
}

/// Diagnostic ping from the frontend — writes a single JSONL line to
/// widget.jsonl in the same envelope shape as backend tracing events. See
/// `logging::FrontendLogger` for why this bypasses the tracing macros.
#[tauri::command]
pub fn frontend_log(
    level: String,
    message: String,
    data: serde_json::Value,
    logger: State<crate::logging::FrontendLogger>,
) {
    logger.log(&level, &message, data);
}

#[tauri::command]
pub fn hide_window(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())?;
    // The About modal is parented to main; hide it too so it doesn't linger
    // as an orphan window after the user dismisses the dashboard.
    if let Some(about) = app.get_webview_window("about") {
        let _ = about.hide();
    }
    Ok(())
}

#[tauri::command]
pub fn show_window(window: WebviewWindow) -> Result<(), String> {
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    // The webview can invoke get_config before setup() finishes managing
    // ConfigState, in which case it receives Config::default() (auto_resize
    // None) and stays stuck there forever. The frontend registers its
    // config_updated listener before calling show_window, so re-pushing the
    // now-authoritative config here corrects any value lost to that race.
    emit_config_updated(window.app_handle());
    Ok(())
}

#[tauri::command]
pub fn toggle_window(window: WebviewWindow) -> Result<(), String> {
    let visible = window.is_visible().map_err(|e| e.to_string())?;
    if visible {
        window.hide().map_err(|e| e.to_string())
    } else {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn quit_app(_app: AppHandle) {
    tracing::info!("quit_app invoked");
    std::process::exit(0);
}

/// Information the onboarding panel needs from Rust: the path to the deployed
/// hook script, the ready-to-paste settings.json snippet, and whether any
/// hook event has ever been received (the panel hides as soon as one has).
#[derive(Serialize)]
pub struct SetupState {
    pub hook_script_path: String,
    pub settings_snippet: String,
    pub has_history: bool,
}

#[tauri::command]
pub fn get_setup_state(app: AppHandle) -> SetupState {
    let hook_path_display = app
        .path()
        .app_data_dir()
        .map(|d| setup::path_for_snippet(&d.join(setup::HOOK_SCRIPT_FILENAME)))
        .unwrap_or_default();
    let settings_snippet = setup::build_settings_snippet(&hook_path_display);
    let has_history = app
        .try_state::<PromptHistoryStore>()
        .map(|s| s.has_any_entries())
        .unwrap_or(false);
    SetupState {
        hook_script_path: hook_path_display,
        settings_snippet,
        has_history,
    }
}

#[tauri::command]
pub fn open_hook_script_location(app: AppHandle) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    open::that(dir).map_err(|e| e.to_string())
}

/// Open the GitHub Pages install / Claude-Code-setup guide in the user's
/// default browser. URL is hard-coded so the command can't be abused as a
/// generic URL opener from the frontend.
#[tauri::command]
pub fn open_setup_docs() -> Result<(), String> {
    open::that("https://anothersava.github.io/claude-code-dashboard/pages/install")
        .map_err(|e| e.to_string())
}

/// Open the GitHub Pages documentation home in the user's default browser.
#[tauri::command]
pub fn open_docs_home() -> Result<(), String> {
    open::that("https://anothersava.github.io/claude-code-dashboard/")
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct AboutInfo {
    pub version: String,
    pub release_date: String,
    pub docs_url: String,
}

/// Convert the build-time ISO date `YYYY-MM-DD` (embedded by `build.rs`) into
/// the human-facing form `Month D, YYYY` (e.g. "May 28, 2026"). Empty input
/// or a parse failure returns an empty string so the About dialog hides the
/// line gracefully.
fn release_date_pretty() -> String {
    let raw = env!("APP_RELEASE_DATE");
    if raw.is_empty() {
        return String::new();
    }
    match chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        Ok(d) => {
            use chrono::Datelike;
            format!("{} {}, {}", d.format("%B"), d.day(), d.year())
        }
        Err(_) => raw.to_string(),
    }
}

#[tauri::command]
pub fn get_about_info(app: AppHandle) -> AboutInfo {
    AboutInfo {
        version: app.package_info().version.to_string(),
        release_date: release_date_pretty(),
        docs_url: "anothersava.github.io/claude-code-dashboard".to_string(),
    }
}

#[tauri::command]
pub fn open_about(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("about") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Right-edge padding (physical pixels) preserved when an anchored window
/// (e.g. the bottom-right main widget) grows. Mirrors
/// `config_watcher::apply_default_position`'s margin so the resized window
/// keeps the same standoff from the work-area edge.
const RESIZE_RIGHT_MARGIN: i32 = 16;

/// Resize a window to (`logical_width`, `logical_height`) CSS pixels.
/// `recenter`:
///   - `false` (e.g. main widget): if the new right edge would intrude into
///     the reserved right margin, shift left so the standoff is preserved.
///     Y is left alone.
///   - `true` (e.g. modal About): re-center the window on its current
///     monitor along both axes after the resize, so growth doesn't slide it
///     off-center.
/// We size first, then read back the *actual* outer size (Tauri's set_size
/// adds a Windows DWM non-client frame even on `decorations: false` windows
/// so the post-resize outer is a bit wider/taller than what was asked for)
/// before computing the final position. Caller is responsible for clamping
/// the requested dimensions to sensible upper bounds.
#[tauri::command]
pub fn set_window_size(
    label: String,
    logical_width: f64,
    logical_height: f64,
    recenter: bool,
    app: AppHandle,
) -> Result<(), String> {
    let Some(window) = app.get_webview_window(&label) else {
        return Ok(());
    };
    window
        .set_size(tauri::LogicalSize::new(logical_width, logical_height))
        .map_err(|e| e.to_string())?;

    let pos = window.outer_position().map_err(|e| e.to_string())?;
    let actual_size = window.outer_size().map_err(|e| e.to_string())?;
    let Some(monitor) = window.current_monitor().ok().flatten() else {
        return Ok(());
    };
    let work = monitor.work_area();

    let (new_x, new_y) = if recenter {
        let work_center_x = work.position.x + (work.size.width as i32) / 2;
        let work_center_y = work.position.y + (work.size.height as i32) / 2;
        let half_w = (actual_size.width as i32) / 2;
        let half_h = (actual_size.height as i32) / 2;
        let x = (work_center_x - half_w)
            .max(work.position.x)
            .min(work.position.x + work.size.width as i32 - actual_size.width as i32);
        let y = (work_center_y - half_h)
            .max(work.position.y)
            .min(work.position.y + work.size.height as i32 - actual_size.height as i32);
        (x, y)
    } else {
        let allowed_right = work.position.x + work.size.width as i32 - RESIZE_RIGHT_MARGIN;
        let actual_right = pos.x + actual_size.width as i32;
        let overflow = actual_right - allowed_right;
        let x = if overflow > 0 {
            (pos.x - overflow).max(work.position.x)
        } else {
            pos.x
        };
        (x, pos.y)
    };

    if new_x != pos.x || new_y != pos.y {
        let _ = window.set_position(tauri::PhysicalPosition::new(new_x, new_y));
    }
    Ok(())
}

#[tauri::command]
pub fn remove_session(id: String, app: AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        state.apply_clear(&id);
    }
    if let Some(reg) = app.try_state::<WatcherRegistry>() {
        reg.stop(&id);
    }
    if let Some(store) = app.try_state::<crate::prompt_history::PromptHistoryStore>() {
        store.remove(&id);
        store.save_to_disk();
    }
    emit_sessions_updated(&app);
}

/// The history window's OS title bar: the user's custom name for the chat, or
/// the chat_id when unnamed.
fn history_title(app: &AppHandle, chat_id: &str) -> String {
    app.try_state::<CustomNamesStore>()
        .and_then(|names| names.get(chat_id))
        .unwrap_or_else(|| chat_id.to_string())
}

#[tauri::command]
pub fn open_history(id: String, app: AppHandle) -> Result<(), String> {
    if let Some(target) = app.try_state::<HistoryTarget>() {
        *target.0.lock().unwrap() = Some(id.clone());
    }
    if let Some(window) = app.get_webview_window("history") {
        let _ = window.set_title(&history_title(&app, &id));
        let _ = window.emit("history_target", &id);
        if let Some(cfg) = app.try_state::<crate::config::ConfigState>() {
            let snap = cfg.snapshot();
            if snap.save_window_position {
                if let Some(pos) = snap.history_window_position {
                    let _ = window.set_position(tauri::PhysicalPosition::new(pos.x, pos.y));
                    if let (Some(w), Some(h)) = (pos.width, pos.height) {
                        let _ = window.set_size(tauri::PhysicalSize::new(w, h));
                    }
                }
            }
        }
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub struct HistoryTarget(pub std::sync::Mutex<Option<String>>);

#[tauri::command]
pub fn get_history_target(state: State<HistoryTarget>) -> Option<String> {
    state.0.lock().unwrap().clone()
}

#[tauri::command]
pub fn get_window_label(window: WebviewWindow) -> String {
    window.label().to_string()
}

#[tauri::command]
pub fn close_window(window: WebviewWindow) -> Result<(), String> {
    use tauri::Emitter;
    window.hide().map_err(|e| e.to_string())?;
    if window.label() == "history" {
        let _ = window.emit("history_hidden", ());
    }
    Ok(())
}

#[tauri::command]
pub fn hide_history(app: AppHandle) -> Result<(), String> {
    use tauri::Emitter;
    if let Some(window) = app.get_webview_window("history") {
        window.hide().map_err(|e| e.to_string())?;
        let _ = window.emit("history_hidden", ());
    }
    Ok(())
}

#[tauri::command]
pub fn set_history_font_size(size: crate::config::HistoryFontSize, app: AppHandle) {
    if let Some(state) = app.try_state::<crate::config::ConfigState>() {
        state.with_mut(|c| c.history_font_size = size);
        let _ = state.save_to_disk();
    }
    crate::tray::sync_history_font_checks(&app, size);
    emit_config_updated(&app);
}

/// Set or clear a user display name for a chat_id. Empty/whitespace clears
/// it (reverts to the chat_id). Keyed by chat_id so it persists across
/// sessions for the same project.
#[tauri::command]
pub fn set_chat_name(chat_id: String, name: String, app: AppHandle) {
    if let Some(names) = app.try_state::<CustomNamesStore>() {
        names.set(&chat_id, &name);
    }
    let history_targets_chat = app
        .try_state::<HistoryTarget>()
        .is_some_and(|target| target.0.lock().unwrap().as_deref() == Some(chat_id.as_str()));
    if history_targets_chat {
        if let Some(window) = app.get_webview_window("history") {
            let _ = window.set_title(&history_title(&app, &chat_id));
        }
    }
    emit_sessions_updated(&app);
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn emit_sessions_updated(app: &AppHandle) {
    let _ = app.emit("sessions_updated", resolved_snapshot(app));
}

pub fn emit_config_updated(app: &AppHandle) {
    if let Some(state) = app.try_state::<ConfigState>() {
        let _ = app.emit("config_updated", state.snapshot());
    }
}

pub fn emit_usage_limits_updated(app: &AppHandle) {
    if let Some(state) = app.try_state::<UsageLimitsState>() {
        let _ = app.emit("usage_limits_updated", state.snapshot());
    }
}

#[tauri::command]
pub async fn test_telegram_notification(app: AppHandle) -> Result<(), String> {
    use crate::notifications::Notifier;

    let cfg = app
        .try_state::<ConfigState>()
        .ok_or_else(|| "config state not initialized".to_string())?
        .snapshot();
    let tg_cfg = cfg
        .notifications
        .as_ref()
        .and_then(|n| n.telegram.as_ref())
        .ok_or_else(|| "no telegram config".to_string())?;

    let notifier = std::sync::Arc::new(TelegramNotifier::new());
    notifier.sync_config(Some(tg_cfg));
    if !notifier.is_enabled() {
        return Err("telegram bot_token and chat_id are required".to_string());
    }

    let handle = notifier
        .send_raw("[dashboard] test — will self-delete in 5s")
        .await
        .map_err(|e| format!("telegram send failed: {e}"))?;

    let notifier_clone = notifier.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if let Err(e) = notifier_clone.dismiss(&handle).await {
            tracing::warn!(?e, handle, "test notification self-delete failed");
        }
    });

    Ok(())
}
