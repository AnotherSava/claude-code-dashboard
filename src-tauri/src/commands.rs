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

/// Snapshot local + remote sessions and fill each `display_name` from the
/// custom-names store. The name rides on the session so the frontend renders
/// it without a separate lookup channel. This is the single point where the
/// local and synced-from-peers worlds combine — remote rows get the overlay
/// too (keyed by their namespaced "{device}/{id}"), so renaming a remote row
/// works and stays a local-only decoration.
fn resolved_snapshot(app: &AppHandle) -> Vec<AgentSession> {
    let Some(state) = app.try_state::<AppState>() else {
        return Vec::new();
    };
    let mut sessions = state.snapshot();
    sessions.extend(state.remote_snapshot());
    if let Some(names) = app.try_state::<CustomNamesStore>() {
        names.apply(&mut sessions);
    }
    sessions
}

#[tauri::command]
pub fn get_sessions(app: AppHandle) -> Vec<AgentSession> {
    resolved_snapshot(&app)
}

#[tauri::command]
pub fn get_config(app: AppHandle) -> Config {
    // ConfigState is managed in the builder chain before `.setup()` and before
    // any webview exists (see lib.rs run()), so it is always present by the time
    // a `get_config` IPC can run. No `unwrap_or_default` fallback — that used to
    // hand back `Config::default()` (auto_resize None) when the webview beat
    // setup()'s late `.manage()`, which the frontend latched and stayed stuck on.
    app.state::<ConfigState>().snapshot()
}

#[tauri::command]
pub fn get_usage_limits(state: State<UsageLimitsState>) -> UsageLimits {
    state.snapshot()
}

#[tauri::command]
pub fn refresh_usage_limits(state: State<UsageLimitsState>) -> bool {
    state.request_refresh()
}

/// Resolve the start of a week in **local time** (Monday 00:00) as ms-epoch.
/// `week_offset` is relative to the current local week: `0` = this week, `-1` =
/// last week, etc. Keeping week alignment here (vs. the client) means the pure,
/// tz-free `build_week_chart` never has to know about timezones.
///
/// DST caveat: the bucket grid is a fixed 7×24×6 layout, so a week containing a
/// clock change is off by ±1h in its final bucket. Acceptable for a personal
/// dashboard.
fn local_week_start_ms(week_offset: i32) -> Result<i64, String> {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let today = Local::now().date_naive();
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let target_monday = monday + Duration::weeks(week_offset as i64);
    let naive_midnight = target_monday.and_hms_opt(0, 0, 0).ok_or("invalid week start")?;
    let dt = Local
        .from_local_datetime(&naive_midnight)
        .earliest()
        .or_else(|| Local.from_local_datetime(&naive_midnight).latest())
        .ok_or("could not resolve local week start")?;
    Ok(dt.timestamp_millis())
}

/// Local usage samples unioned with every synced peer's, sorted ascending by
/// `ts`. The 5h/7d counter is account-wide, so a peer's polls during the
/// windows this device's app was closed describe the same timeline — merging
/// them fills the Work-intensity chart's gaps (`build_week_chart` walks the
/// combined timeline and clamps each step to a non-negative delta, so the
/// extra interleaved points are harmless where coverage overlaps). Tolerant of
/// either store being absent.
fn merged_usage_records(app: &AppHandle) -> Vec<crate::usage_history::UsageHistoryRecord> {
    let mut records = app
        .try_state::<crate::usage_history::UsageHistoryStore>()
        .map(|s| s.read_all())
        .unwrap_or_default();
    if let Some(remote) = app.try_state::<crate::remote_usage::RemoteUsageStore>() {
        records.extend(remote.all_records());
    }
    records.sort_by_key(|r| r.ts);
    records
}

/// Build the work-intensity chart for one week (see `local_week_start_ms`).
#[tauri::command]
pub fn get_usage_intensity_week(week_offset: i32, app: AppHandle) -> Result<crate::usage_history::WeekChart, String> {
    let week_start_ms = local_week_start_ms(week_offset)?;
    let records = merged_usage_records(&app);
    Ok(crate::usage_history::build_week_chart(&records, week_start_ms))
}

/// Build a chart for every week from the current one back to the week that holds
/// the oldest record, newest first. Powers the "by week" overview (one row per
/// week). Reads the history once and reuses it across weeks.
#[tauri::command]
pub fn get_usage_intensity_weeks(app: AppHandle) -> Result<Vec<crate::usage_history::WeekChart>, String> {
    let records = merged_usage_records(&app);
    let Some(first) = records.first() else {
        return Ok(Vec::new());
    };
    let data_min = first.ts;
    let mut weeks = Vec::new();
    let mut offset = 0;
    loop {
        let week_start_ms = local_week_start_ms(offset)?;
        weeks.push(crate::usage_history::build_week_chart(&records, week_start_ms));
        if week_start_ms <= data_min {
            break; // this week already covers the oldest record
        }
        offset -= 1;
        if offset < -520 {
            break; // ~10-year safety cap against an absurd clock
        }
    }
    Ok(weeks)
}

#[tauri::command]
pub fn apply_auto_resize(physical_height: f64, app: AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let mode = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().auto_resize)
        .unwrap_or_default();
    if let Err(e) = crate::auto_resize::apply(&window, mode, physical_height) {
        tracing::warn!(?e, physical_height, "apply_auto_resize failed");
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

/// Set when the app was auto-launched at login in "Open to tray" mode. While
/// it stays set, the two automatic reveal paths — the frontend's mount-time
/// `show_window` call and the safety-net timer in `lib.rs` — keep the main
/// window hidden, so the app lives in the tray. The tray "Show / Hide" entry
/// and `toggle_window` call `window.show()` directly and are unaffected.
pub struct SuppressInitialShow(pub std::sync::atomic::AtomicBool);

#[tauri::command]
pub fn show_window(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    if let Some(suppress) = app.try_state::<SuppressInitialShow>() {
        if suppress.0.load(std::sync::atomic::Ordering::SeqCst) {
            // Started minimized to tray: swallow the frontend's auto-reveal but
            // still re-push setup_state (its get_setup_state read can race
            // setup() managing PromptHistoryStore, flashing the onboarding panel).
            emit_setup_state(&app);
            return Ok(());
        }
    }
    ensure_window_on_screen(&window);
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    // Re-push setup_state: `get_setup_state` can run before setup() manages
    // PromptHistoryStore and latch has_history:false. The frontend registers its
    // setup_state listener before calling show_window, so this corrects it.
    // Config no longer needs re-pushing — ConfigState is managed before the
    // webview loads (see lib.rs run()), so `get_config` can't race to `None`.
    emit_setup_state(window.app_handle());
    Ok(())
}

#[tauri::command]
pub fn toggle_window(window: WebviewWindow) -> Result<(), String> {
    let visible = window.is_visible().map_err(|e| e.to_string())?;
    if visible {
        window.hide().map_err(|e| e.to_string())
    } else {
        ensure_window_on_screen(&window);
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
#[derive(Serialize, Clone)]
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

/// Move a window to a persisted `WindowPosition`, and resize to it when the
/// saved geometry included a size (older configs stored only x/y). Best-effort
/// — restoration errors are swallowed.
pub fn apply_window_position(window: &WebviewWindow, pos: &crate::config::WindowPosition) {
    let _ = window.set_position(tauri::PhysicalPosition::new(pos.x, pos.y));
    if let (Some(w), Some(h)) = (pos.width, pos.height) {
        let _ = window.set_size(tauri::PhysicalSize::new(w, h));
    }
}

/// Minimum overlap (physical px, per axis) between the window and some
/// monitor's work area for the window to count as reachable — enough that the
/// user can both see it and grab its drag region. A window narrower/shorter
/// than this can't be asked to overlap by more than its own span.
const MIN_ONSCREEN_OVERLAP: i32 = 64;

/// Rescue a window that has drifted entirely off every connected monitor.
///
/// A saved `window_position` is restored verbatim (`apply_window_position`),
/// and a window that was on-screen keeps its physical coordinates across a
/// monitor unplug / resolution / DPI change. Either way it can end up floating
/// in dead space where it's invisible *and* immovable — its drag region is
/// off-screen too — so the tray Show/Hide just toggles a window nobody can see.
/// Detect that and pull the window back onto the monitor it overlaps most (the
/// primary when it overlaps none), clamped fully into that work area. Returns
/// true if it moved. Call after any position restore and on every show path.
pub fn ensure_window_on_screen(window: &WebviewWindow) -> bool {
    use crate::auto_resize::WorkAreaBounds;
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return false;
    };
    let (w, h) = (size.width as i32, size.height as i32);
    let Ok(monitors) = window.available_monitors() else {
        return false;
    };
    if monitors.is_empty() {
        return false;
    }
    let bounds: Vec<WorkAreaBounds> = monitors.iter().map(WorkAreaBounds::from_monitor).collect();

    // Reachable if some work area overlaps the window by a usable patch on both
    // axes — a thin sliver poking onto a screen isn't grabbable, so it doesn't
    // count as on-screen.
    let reachable = bounds.iter().any(|b| {
        b.overlap_x(pos.x, w) >= MIN_ONSCREEN_OVERLAP.min(w)
            && b.overlap_y(pos.y, h) >= MIN_ONSCREEN_OVERLAP.min(h)
    });
    if reachable {
        return false;
    }

    // Off-screen — prefer the monitor it already overlaps most; fall back to the
    // primary when it overlaps none at all, then to the first connected one.
    let target = bounds
        .iter()
        .copied()
        .filter(|b| b.intersection_area(pos.x, pos.y, w, h) > 0)
        .max_by_key(|b| b.intersection_area(pos.x, pos.y, w, h))
        .or_else(|| window.primary_monitor().ok().flatten().map(|m| WorkAreaBounds::from_monitor(&m)))
        .unwrap_or(bounds[0]);
    let (x, y) = target.clamp(pos.x, pos.y, w, h);
    tracing::info!(
        label = %window.label(),
        from = ?(pos.x, pos.y),
        to = ?(x, y),
        size = ?(w, h),
        monitors = monitors.len(),
        "ensure_window_on_screen: window was off every monitor, pulled back",
    );
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    true
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
///     Y is left alone except by the work-area clamp.
///   - `true` (e.g. modal About): re-center the window on its current
///     monitor along both axes, so growth doesn't slide it off-center.
///
/// Ordering: we call `set_position` BEFORE `set_size`. The reverse causes a
/// visible flicker on macOS — `set_size` grows the window past the work-area
/// floor/edge, then `set_position` shifts it back into view a frame or two
/// later. Moving first means the intermediate state (new position, old size)
/// is always on-screen.
///
/// Sizing: we compute the new outer rect from `requested × scale` instead of
/// reading `outer_size()` — pre-resize that gives the *old* size (wrong for
/// growth), and post-resize on macOS it can lag by several frames (the bug
/// that left the window off-screen before this rewrite). On Windows the DWM
/// non-client frame adds ~7px even on `decorations: false`, so the actual
/// right edge after `set_size` may land that far past where we computed —
/// well within `RESIZE_RIGHT_MARGIN` (16px), so still inside the work area.
/// Caller is responsible for clamping the requested dimensions to sensible
/// upper bounds.
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
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let pos = window.outer_position().map_err(|e| e.to_string())?;
    let Some(monitor) = window.current_monitor().ok().flatten() else {
        // No monitor — just size and bail (can't compute work area).
        window
            .set_size(tauri::LogicalSize::new(logical_width, logical_height))
            .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let work = monitor.work_area();
    let bounds = crate::auto_resize::WorkAreaBounds::from_monitor(&monitor);
    let new_w = (logical_width * scale).round() as i32;
    let new_h = (logical_height * scale).round() as i32;

    let (raw_x, raw_y) = if recenter {
        let work_center_x = work.position.x + (work.size.width as i32) / 2;
        let work_center_y = work.position.y + (work.size.height as i32) / 2;
        (work_center_x - new_w / 2, work_center_y - new_h / 2)
    } else {
        let allowed_right = work.position.x + work.size.width as i32 - RESIZE_RIGHT_MARGIN;
        let actual_right = pos.x + new_w;
        let overflow = actual_right - allowed_right;
        let x = if overflow > 0 { pos.x - overflow } else { pos.x };
        (x, pos.y)
    };
    let (new_x, new_y) = bounds.clamp(raw_x, raw_y, new_w, new_h);
    tracing::debug!(
        label = %label,
        logical = ?(logical_width, logical_height),
        scale,
        new_size = ?(new_w, new_h),
        pos = ?(pos.x, pos.y),
        target = ?(new_x, new_y),
        moved = new_x != pos.x || new_y != pos.y,
        "set_window_size",
    );

    // Position first (always on-screen intermediate state), then resize.
    if new_x != pos.x || new_y != pos.y {
        let _ = window.set_position(tauri::PhysicalPosition::new(new_x, new_y));
    }
    window
        .set_size(tauri::LogicalSize::new(logical_width, logical_height))
        .map_err(|e| e.to_string())?;
    Ok(())
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
    // Remote sessions accumulate dialog from push deltas, which a dashboard
    // restart discards — catch up from the origin device now so the window
    // fills in once the fetch lands (it re-emits and the window re-renders).
    // No-op for local ids: no remote device prefix matches.
    crate::sync::fetch_remote_dialog(app.clone(), id.clone());
    if let Some(window) = app.get_webview_window("history") {
        let _ = window.set_title(&history_title(&app, &id));
        let _ = window.emit("history_target", &id);
        let snap = app.try_state::<crate::config::ConfigState>().map(|cfg| cfg.snapshot()).filter(|snap| snap.save_window_position);
        let saved = snap.as_ref().and_then(|s| s.history_window_position);
        let want_maximized = snap.as_ref().is_some_and(|s| s.history_window_maximized);
        // Closing the window only hides it — a maximized window stays maximized
        // while hidden. So on reopen it's usually already in the right state;
        // touching geometry only when it differs avoids flashing a normal-size
        // frame before re-maximizing (unmaximize → resize → maximize).
        let already_maximized = window.is_maximized().unwrap_or(false);
        match (saved, want_maximized) {
            (Some(pos), false) => {
                let _ = window.unmaximize();
                apply_window_position(&window, &pos);
            }
            (Some(pos), true) if !already_maximized => {
                // Set the unmaximized bounds first so they become the
                // restore-rect for a later un-maximize, then maximize.
                apply_window_position(&window, &pos);
                let _ = window.maximize();
            }
            (Some(_), true) => {} // already maximized — leave it, no flash
            (None, _) if !already_maximized => {
                // No saved bounds: open maximized on the dashboard's monitor.
                if let Some(monitor) = app.get_webview_window("main").and_then(|m| m.current_monitor().ok().flatten()) {
                    let _ = window.set_position(*monitor.position());
                }
                let _ = window.maximize();
            }
            (None, _) => {} // no saved bounds, already maximized
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

/// Remove a local session row exactly as a `SessionEnd` would — append a
/// history separator, persist the final dialog, drop the in-memory row, stop its
/// transcript watcher and owning-pid tracking, and emit. Shared by the
/// `SessionEnd` Clear branch ([`crate::http_server`]) and the liveness reaper
/// ([`crate::liveness_reaper`]) so the two removal paths can't drift apart.
///
/// `expect_updated`, when `Some`, makes the removal abort (returns `false`) if
/// the row received a new event since it was observed — the reaper passes the
/// row's last-seen `updated` to close the reap-vs-restart race; the Clear branch
/// passes `None` (it is reacting to an authoritative event). Returns whether a
/// row was actually removed.
pub fn remove_session(app: &AppHandle, id: &str, expect_updated: Option<i64>, now: i64) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    let Some(removed) = state.take_session(id, expect_updated, now) else {
        return false;
    };
    // Persist the final dialog (now ending in a separator) so the next
    // SessionStart for this cwd restores history that already ends at the
    // boundary — the same continuity `/clear` relies on.
    if let Some(h) = app.try_state::<PromptHistoryStore>() {
        h.save_session(&removed);
        h.save_to_disk();
    }
    if let Some(reg) = app.try_state::<WatcherRegistry>() {
        reg.stop(id);
    }
    if let Some(pids) = app.try_state::<crate::liveness::AgentPids>() {
        pids.forget(id);
    }
    emit_sessions_updated(app);
    true
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn emit_sessions_updated(app: &AppHandle) {
    let sessions = resolved_snapshot(app);
    // Every state transition flows through this emit, so it doubles as the
    // single trigger for terminal tab-title reconciliation — the tab tracks
    // exactly what the row shows (watcher promotions, renames, removals)
    // without a second state machine. Titles are a local-machine concern:
    // hand over only the local subset so remote rows can't even reach the
    // pid bookkeeping.
    let local: Vec<AgentSession> = sessions.iter().filter(|s| s.origin.is_none()).cloned().collect();
    crate::terminal_title::sync(app, &local);
    // Per-session context usage feeds the tray's context-alert border, so this
    // emit chokepoint also keeps the tray icon in step as token counts change.
    crate::tray_badge::refresh(app);
    let _ = app.emit("sessions_updated", sessions);
    // ...and it doubles again as the sync-push trigger: the pusher debounces
    // pokes and ships *local* sessions to peers. Remote-driven changes must go
    // through `emit_sessions_updated_remote` instead — the *content* of a
    // received push can't echo (remote sessions are never re-broadcast), but
    // the poke itself would: receive → poke → push our locals back → the peer
    // receives and pokes its own pusher, ping-ponging full snapshots at the
    // debounce period forever.
    if let Some(dirty) = app.try_state::<crate::sync::SyncDirty>() {
        dirty.inner().0.notify_one();
    }
}

/// UI-only refresh for remote-driven changes (received sync push, remote TTL
/// reap, dialog catch-up). Skips the `SyncDirty` poke — those paths mutate
/// only `AppState::remote`, which the pusher never ships, and poking it back
/// would ping-pong pushes between two devices (see `emit_sessions_updated`).
/// Also skips terminal-title reconciliation: remote rows never own a local
/// terminal, and the local subset is untouched by definition.
pub fn emit_sessions_updated_remote(app: &AppHandle) {
    let _ = app.emit("sessions_updated", resolved_snapshot(app));
}

pub fn emit_config_updated(app: &AppHandle) {
    if let Some(state) = app.try_state::<ConfigState>() {
        let _ = app.emit("config_updated", state.snapshot());
    }
}

/// Push the authoritative setup state to the frontend. Like `get_config`,
/// `get_setup_state` can be invoked at mount before `setup()` has managed
/// `PromptHistoryStore`, returning `has_history: false` and flashing the
/// onboarding panel on a configured install. The frontend registers its
/// `setup_state` listener before calling `show_window`, so re-pushing from there
/// reliably corrects any value lost to that race.
pub fn emit_setup_state(app: &AppHandle) {
    let _ = app.emit("setup_state", get_setup_state(app.clone()));
}

pub fn emit_usage_limits_updated(app: &AppHandle) {
    if let Some(state) = app.try_state::<UsageLimitsState>() {
        let _ = app.emit("usage_limits_updated", state.snapshot());
    }
    // Keep the tray badge/tooltip in step with every usage update.
    crate::tray_badge::refresh(app);
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
