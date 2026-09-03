use std::collections::HashMap;

use crate::config::{Config, ConfigState};
use crate::custom_names::CustomNamesStore;
use crate::log_watcher::WatcherRegistry;
use crate::prompt_history::PromptHistoryStore;
use crate::setup;
use crate::state::{AgentSession, AppState, Attention, Canary, Status};
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
pub(crate) fn resolved_snapshot(app: &AppHandle) -> Vec<AgentSession> {
    let Some(state) = app.try_state::<AppState>() else {
        return Vec::new();
    };
    let mut sessions = state.snapshot();
    sessions.extend(state.remote_snapshot());
    if let Some(names) = app.try_state::<CustomNamesStore>() {
        names.apply(&mut sessions);
    }
    // Stamp the canary status for local rows from the live nonce store so the
    // frontend can color the agent name. `Alive` requires the marker to have been
    // *observed* at least once (the nonce's `seen` bit) — a set-up-but-unconfirmed
    // session is `Pending`, not `Alive`, so the green never over-claims. A disabled
    // feature, a pre-feature session, or a post-restart row (in-memory nonce lost)
    // all read `Off`. Remote rows stay `Off` — this device isn't running the canary
    // for them.
    if let (Some(ns), Some(cfg_state)) = (app.try_state::<crate::nonce_store::NonceStore>(), app.try_state::<ConfigState>()) {
        let enabled = cfg_state.config.lock().unwrap().instruction_canary_enabled;
        for s in sessions.iter_mut().filter(|s| s.origin.is_none()) {
            s.canary = match ns.get(&s.id) {
                Some((_, seen)) if enabled => {
                    if s.instruction_drift {
                        Canary::Dead
                    } else if seen {
                        Canary::Alive
                    } else {
                        Canary::Pending
                    }
                }
                _ => Canary::Off,
            };
        }
    }
    stamp_name_sharing(app, &mut sessions);
    sessions
}

/// Count how many live sessions answer to each local row's *name*, for
/// [`AgentSession::name_shared_by`].
///
/// Must run after the custom names are applied, since the name in question is
/// `display_label` — what `terminal_title::build_title` puts on the tab, and
/// therefore the string two tabs can end up sharing.
///
/// Two sources, one number, because both produce the same thing: identically
/// captioned tabs. Several sessions can derive **one** row — a
/// `--fork-session --resume` migration, or, with `projects_root` unset, two
/// projects whose directory basenames match — and `terminal_title::sync` titles
/// whichever of them fired the last hook, so both tabs end up carrying that one
/// row's caption. Separately, two **rows** can be renamed alike, which
/// `custom_names::set` does not prevent. Summing the per-row counts across a
/// shared label covers both without asking which happened.
///
/// This is what `attention::resolve_row` is up against, and it is worth more than
/// the attention gap: everything keyed on a row id is guessing which conversation
/// it means — the transcript watcher restarts on whichever session spoke last,
/// the dialogs interleave, `tab_pid` and `inbox_for` refuse, and the liveness
/// reaper can drop a row a live session still holds.
fn stamp_name_sharing(app: &AppHandle, sessions: &mut [AgentSession]) {
    let Some(registry) = app.try_state::<crate::session_registry::SessionRegistry>() else { return };
    // The config guard is taken and released before the registry call:
    // `SessionRegistry::refresh` does a directory read plus a full process
    // enumeration on a cache miss, and holding `ConfigState` across that would
    // put `config_watcher` behind blocking I/O.
    let root = app.try_state::<ConfigState>().and_then(|c| c.config.lock().unwrap().projects_root.clone());
    let anchors = app.try_state::<crate::chat_id_registry::ChatIdRegistry>();
    let anchored = |sid: &str| anchors.as_ref().and_then(|r| r.anchored(sid));
    // Unreadable stays unreadable: leaving every row `None` says "not
    // established", where writing 1 everywhere would assert a uniqueness nothing
    // checked.
    let Some(live) = registry.live_sessions(root.as_deref(), now_ms()) else { return };
    // Keyed by the *anchored* row id, the same resolution `agent_roster` and
    // `session_restore` perform: a `cd`-ed session derives another row's id, and
    // counting under the raw derivation files it against a row it is not in.
    let mut per_row: HashMap<String, usize> = HashMap::new();
    for s in &live {
        *per_row.entry(s.row_id(&anchored)).or_default() += s.sessions;
    }
    let per_name = name_counts(sessions, &per_row);
    for s in sessions.iter_mut().filter(|s| s.origin.is_none()) {
        let shared = per_name.get(s.display_label()).copied();
        s.name_shared_by = shared;
    }
}

/// How many things answer to each local row's display label, given how many live
/// sessions the registry maps to each row.
///
/// Pure, because the floor is the subtle part: **a row counts at least once even
/// where the registry knows no session for it**. That is what keeps the number in
/// step with `attention::resolve_row`, which refuses two rows sharing a label
/// whether or not either has a live session behind it. Summing bare session counts
/// would leave the marker silent on exactly the rows the sensor had already given
/// up on — a warning that lies by staying quiet, which is worse than no warning.
fn name_counts(sessions: &[AgentSession], per_row: &HashMap<String, usize>) -> HashMap<String, usize> {
    let mut per_name: HashMap<String, usize> = HashMap::new();
    for s in sessions.iter().filter(|s| s.origin.is_none()) {
        *per_name.entry(s.display_label().to_string()).or_default() += per_row.get(&s.id).copied().unwrap_or(0).max(1);
    }
    per_name
}

/// The snapshot everything the *user* looks at is built from: [`resolved_snapshot`]
/// plus [`apply_read_as_idle`].
///
/// The split is the point. `resolved_snapshot` answers "what is each agent doing",
/// which is what `/api/agents` and the sync push report; this answers "what should
/// this person see", which additionally depends on whether they have already read
/// it. Only the frontend and the terminal titles want the second question.
pub(crate) fn display_snapshot(app: &AppHandle) -> Vec<AgentSession> {
    let mut sessions = resolved_snapshot(app);
    if app.try_state::<ConfigState>().is_some_and(|c| c.config.lock().unwrap().attention_tracking) {
        apply_read_as_idle(&mut sessions);
    }
    sessions
}

/// Show a finished local row the user has already read as `Idle`.
///
/// `Done` in this dashboard means "finished, **and you haven't looked**" — once
/// you have, the row is just a live session sitting there, which is what `Idle`
/// already means. Folding read/unread into the status vocabulary rather than
/// adding a badge beside it is why there is no second signal on the row: the
/// widget's pill and `terminal_title`'s ⚪-vs-🟢 both fall out of the status.
///
/// Derived at display time rather than written into `AppState`, which buys three
/// things. A machine reading `/api/agents` or a sync push still learns what the
/// *agent* did, not whether a human at this keyboard has looked at their screen.
/// `state_entered_at` is untouched, so the row keeps counting from when the agent
/// finished instead of restarting from when it was read. And the late-flush case
/// needs no machinery: `Stop` settles a row `Done` before Claude Code writes the
/// final reply, so a read landing in that gap is undone for free the moment the
/// text arrives and moves `content_at` past the stamp — the row simply reads
/// `Done` again at the next emit.
pub(crate) fn apply_read_as_idle(sessions: &mut [AgentSession]) {
    for s in sessions.iter_mut().filter(|s| s.origin.is_none() && s.status == Status::Done) {
        if s.attention() == Attention::Seen {
            s.status = Status::Idle;
        }
    }
}

#[tauri::command]
pub fn get_sessions(app: AppHandle) -> Vec<AgentSession> {
    display_snapshot(&app)
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

/// Every token record this device holds, reduced to one row per `message.id`.
///
/// Reduction is mandatory rather than defensive: Claude Code writes one
/// transcript line per content block of a response, each repeating the whole
/// usage object, so summing the stored rows inflates a total by ~2x.
/// Unions this device's records with every peer's, since the chart is meant to
/// show account-wide work and transcripts are per-machine.
fn merged_token_records(app: &AppHandle) -> Vec<crate::token_history::TokenRecord> {
    let mut all = match app.try_state::<crate::token_history::TokenHistoryStore>() {
        Some(store) => store.read_all(),
        None => Vec::new(),
    };
    if let Some(remote) = app.try_state::<crate::remote_tokens::RemoteTokenStore>() {
        all.extend(remote.all_records());
    }
    // Reducing after the union matters: the same `message.id` can arrive from
    // two devices (one scanned it, one received it), and only a reduction over
    // the combined set collapses that.
    crate::token_history::reduce_by_id(&all)
}

/// Full-height value for one 10-minute token bar, from config.
fn token_axis_max(app: &AppHandle) -> f64 {
    app.try_state::<ConfigState>()
        .and_then(|c| c.snapshot().intensity_axis_max_tokens)
        .filter(|v| *v > 0.0)
        .unwrap_or(crate::token_history::DEFAULT_AXIS_MAX_TOKENS)
}

/// Token-unit twin of [`get_usage_intensity_week`], for the chart's Tokens view.
#[tauri::command]
pub fn get_token_intensity_week(week_offset: i32, app: AppHandle) -> Result<crate::token_history::TokenWeekChart, String> {
    let week_start_ms = local_week_start_ms(week_offset)?;
    let records = merged_token_records(&app);
    Ok(crate::token_history::build_token_week_chart(&records, week_start_ms, token_axis_max(&app)))
}

/// Token-unit twin of [`get_usage_intensity_weeks`]. Reads and reduces the
/// history once, then reuses it across every week.
#[tauri::command]
pub fn get_token_intensity_weeks(app: AppHandle) -> Result<Vec<crate::token_history::TokenWeekChart>, String> {
    let records = merged_token_records(&app);
    let Some(data_min) = records.iter().map(|r| r.ts).min() else {
        return Ok(Vec::new());
    };
    let axis_max = token_axis_max(&app);
    let mut weeks = Vec::new();
    let mut offset = 0;
    loop {
        let week_start_ms = local_week_start_ms(offset)?;
        weeks.push(crate::token_history::build_token_week_chart(&records, week_start_ms, axis_max));
        if week_start_ms <= data_min {
            break;
        }
        offset -= 1;
        if offset < -520 {
            break; // ~10-year safety cap against an absurd clock
        }
    }
    Ok(weeks)
}

/// Resize the main window to fit `physical_height` physical px. The frontend
/// sizes against the webview's own `devicePixelRatio` (the ratio it rasterizes
/// content at), which — unlike Rust's `window.scale_factor()` — tracks the
/// window landing on a different-DPI monitor, so nothing round-trips back here.
#[tauri::command]
pub fn apply_auto_resize(physical_height: f64, app: AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
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
/// goes through `toggle_main` -> `reveal`, which doesn't consult this flag, so
/// the user can still surface the window whenever they ask for it.
pub struct SuppressInitialShow(pub std::sync::atomic::AtomicBool);

#[tauri::command]
pub fn show_window(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    if let Some(suppress) = app.try_state::<SuppressInitialShow>() {
        if suppress.0.load(std::sync::atomic::Ordering::SeqCst) {
            // Started minimized to tray: swallow the frontend's auto-reveal.
            return Ok(());
        }
    }
    reveal(&window)?;
    // No state re-push: every store the frontend reads at mount (ConfigState,
    // PromptHistoryStore, …) is managed before the webview exists (see lib.rs
    // run()'s build()/run() gap), so get_config / get_setup_state can't race a
    // default. The former config/setup_state re-emit backstops are gone.
    Ok(())
}

/// Bring a window fully back into view.
///
/// `show()` alone is not enough for a *minimized* window: on Windows it maps to
/// `ShowWindow(SW_SHOW)`, which redisplays the window in its current state — so
/// an iconic window is revealed as the iconic rect, a header-sized strip. That
/// is what the tray's Show produced after a full-screen game had minimized the
/// widget. Un-minimize first, which also gives `ensure_window_on_screen` a real
/// rect to judge instead of the off-every-monitor iconic one.
pub(crate) fn reveal(window: &WebviewWindow) -> Result<(), String> {
    if crate::auto_resize::is_minimized(window) {
        window.unminimize().map_err(|e| e.to_string())?;
    }
    ensure_window_on_screen(window);
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())
}

/// Show or hide the widget — the one implementation behind both entry points.
///
/// This deliberately lives here rather than next to either caller, because there
/// used to be two copies of it: this one behind the `toggle_window` command, and
/// a private twin in `tray.rs`. They drifted, and the drift was invisible — the
/// command has no frontend caller, so every real click went through the tray's
/// copy, and a fix applied here reached nothing the user could trigger.
pub(crate) fn toggle_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    // A minimized window still reports itself visible — minimizing doesn't clear
    // WS_VISIBLE — so the bare visibility check reads a widget some full-screen
    // app had minimized as "already showing" and hides it, costing two tray
    // clicks to get it back (and the second one only revealing the icon, since
    // `show()` alone doesn't un-minimize). Minimized counts as not showing.
    let showing =
        window.is_visible().unwrap_or(true) && !crate::auto_resize::is_minimized(&window);
    if showing {
        let _ = window.hide();
        // Carry the About modal with the dashboard — leaving it visible after
        // the tray hides main produces a stray floating window.
        if let Some(about) = app.get_webview_window("about") {
            let _ = about.hide();
        }
    } else if let Err(e) = reveal(&window) {
        tracing::warn!(error = %e, "toggle_main: reveal failed");
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
    // A minimized window is off every monitor by construction — the OS parks it
    // at the iconic rect, (-32000, -32000) on Windows — so the rescue below
    // would fire on every show-while-minimized and write a position derived
    // from a rect that isn't the window's. There is nothing stranded: the real
    // position comes back with the restore.
    if crate::auto_resize::is_minimized(window) {
        return false;
    }
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

/// Runtime-only state for compact-view width. The widget's width in compact
/// view is a transient display width; the user's real (non-compact) width is
/// remembered here so leaving compact restores it and so the save-on-close path
/// never persists the compact width. Not persisted — captured on the first
/// shrink and cleared on restore, so it only ever holds a value while shrunk.
#[derive(Default)]
pub struct CompactWidth(pub std::sync::Mutex<CompactWidthState>);

#[derive(Default, Clone, Copy)]
pub struct CompactWidthState {
    /// The window is currently shrunk to compact width.
    pub shrunk: bool,
    /// Outer width in physical px captured the moment compact was entered — the
    /// width to restore on exit and to persist (right-anchored) on close. Only
    /// the width is remembered: the compact transform touches width and x only;
    /// height is a separate auto-managed axis, so the save keeps the live height.
    pub non_compact_width: Option<i32>,
}

/// Fit the main window's width to compact view, or restore the remembered
/// non-compact width. Driven by the frontend when `compact_mode` changes, and
/// re-fired while compact whenever the header content (e.g. the usage numbers)
/// changes so the fit stays tight.
///
/// `header_inner_width_phys` is the header's natural content width in physical
/// px, measured in the DOM; `None` on the restore path.
///
/// Width is anchored to the window's right edge — the widget shrinks from the
/// left and keeps its corner. The compact width is never saved: it's a
/// transient transform over the non-compact geometry captured on entry, which
/// the close-time save reconstructs (see `save_window_position_if_enabled`).
#[tauri::command]
pub fn set_compact_width(
    compact: bool,
    header_inner_width_phys: Option<f64>,
    app: AppHandle,
) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    // Same invariant as `auto_resize::apply`: never derive geometry from a
    // minimized window, which reports the OS's iconic rect instead of its own.
    // This is genuinely reachable rather than theoretical — the frontend's
    // width `$effect` keys on `usage`, so a usage poll re-fires it on its own
    // cadence for as long as the widget stays minimized (widget.jsonl recorded
    // exactly that during the incident: `cur_inner_w: 215`, the iconic client
    // width, with `new_x: 0` from the clamp). The width is re-fitted on the
    // restore edge along with the height, so nothing is lost by skipping.
    if crate::auto_resize::is_minimized(&window) {
        tracing::debug!(compact, reason = "minimized", "set_compact_width skipped");
        return Ok(());
    }
    let state = app.state::<CompactWidth>();
    let (Ok(pos), Ok(outer), Ok(inner)) =
        (window.outer_position(), window.outer_size(), window.inner_size())
    else {
        return Ok(());
    };
    // The invisible resize border on frameless Windows makes outer > inner; keep
    // width math in outer space for the right-edge anchor, but set the inner
    // (client) size — the unit `set_size` writes and the frontend measured.
    let frame_w = outer.width as i32 - inner.width as i32;
    let cur_outer_w = outer.width as i32;
    let cur_inner_w = inner.width as i32;
    let cur_inner_h = inner.height;
    let outer_h = outer.height as i32;
    let right_edge = pos.x + cur_outer_w;

    let mut guard = state.0.lock().unwrap();

    if compact {
        let Some(hdr) = header_inner_width_phys else {
            return Ok(());
        };
        // Relax the min-width floor first so the narrow set_size isn't clamped
        // back up. Idempotent — safe to re-assert on every re-measure.
        crate::auto_resize::set_compact_min_width(&window, true);
        // Capture the non-compact width once, on the transition into compact.
        if !guard.shrunk {
            guard.non_compact_width = Some(cur_outer_w);
            guard.shrunk = true;
        }
        let target_inner = (hdr.round() as i32).max(1);
        // Already fitted (a re-measure with unchanged header) — nothing to do.
        if (target_inner - cur_inner_w).abs() <= 1 {
            return Ok(());
        }
        let target_outer = target_inner + frame_w;
        let raw_x = right_edge - target_outer;
        let (new_x, new_y) =
            crate::auto_resize::clamp_to_work_area(&window, raw_x, pos.y, target_outer, outer_h);
        window
            .set_position(tauri::PhysicalPosition::new(new_x, new_y))
            .map_err(|e| e.to_string())?;
        window
            .set_size(tauri::PhysicalSize::new(target_inner as u32, cur_inner_h))
            .map_err(|e| e.to_string())?;
        tracing::debug!(target_inner, cur_inner_w, new_x, decision = "compact_width", "shrunk to header width");
    } else {
        // Restore path: no-op unless we actually shrank.
        if !guard.shrunk {
            return Ok(());
        }
        let ncw = guard.non_compact_width.unwrap_or(cur_outer_w);
        let target_outer = ncw;
        let target_inner = (ncw - frame_w).max(1);
        let raw_x = right_edge - target_outer;
        let (new_x, new_y) =
            crate::auto_resize::clamp_to_work_area(&window, raw_x, pos.y, target_outer, outer_h);
        window
            .set_position(tauri::PhysicalPosition::new(new_x, new_y))
            .map_err(|e| e.to_string())?;
        window
            .set_size(tauri::PhysicalSize::new(target_inner as u32, cur_inner_h))
            .map_err(|e| e.to_string())?;
        guard.shrunk = false;
        guard.non_compact_width = None;
        // Restore the floor after widening (a 300 floor under an already-wider
        // window can't clamp it).
        crate::auto_resize::set_compact_min_width(&window, false);
        tracing::debug!(restored_outer = ncw, new_x, decision = "compact_width", "restored non-compact width");
    }
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
    // Opening a row's history *is* the act of looking at it — the app's only
    // per-session user action. Stamped at both ends (see `hide_history` /
    // `close_window`) so a read spanning a mid-read transcript flush still ends
    // attended rather than being re-raised by its own arriving text.
    let _ = crate::attention::observe(&app, &id, now_ms(), crate::attention::AttentionSource::HistoryOpened);
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
        mark_history_target_read(window.app_handle());
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
    mark_history_target_read(&app);
    Ok(())
}

/// Close half of the read dwell: stamp attention on whichever row the history
/// window was showing. `HistoryTarget` is a single slot written only by
/// `open_history` and never cleared, which is exactly right here — read at the
/// moment a close happens, it is by construction the row that was open.
fn mark_history_target_read(app: &AppHandle) {
    let Some(id) = app.try_state::<HistoryTarget>().and_then(|t| t.0.lock().unwrap().clone()) else {
        return;
    };
    let _ = crate::attention::observe(app, &id, now_ms(), crate::attention::AttentionSource::HistoryClosed);
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

/// Switch the Work intensity chart between the percent-of-quota and token
/// units. Persisted so the choice survives closing the window; no tray item,
/// since the chart carries its own control.
#[tauri::command]
pub fn set_intensity_unit(unit: crate::config::IntensityUnit, app: AppHandle) {
    if let Some(state) = app.try_state::<crate::config::ConfigState>() {
        state.with_mut(|c| c.intensity_unit = unit);
        let _ = state.save_to_disk();
    }
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
    let sessions = display_snapshot(app);
    // Every state transition flows through this emit, so it doubles as the
    // single trigger for terminal tab-title reconciliation — the tab tracks
    // exactly what the row shows (watcher promotions, renames, removals)
    // without a second state machine. Titles are a local-machine concern:
    // hand over only the local subset so remote rows can't even reach the
    // pid bookkeeping.
    let local: Vec<AgentSession> = sessions.iter().filter(|s| s.origin.is_none()).cloned().collect();
    crate::terminal_title::sync(app, &local);
    // Same chokepoint drives the lid-closed sleep veto: it must be armed while
    // an agent is busy *before* the lid shuts, since a lid close sleeps the Mac
    // instantly and leaves no window to react in. Local rows only — a remote
    // row's work is another machine's problem, and its device holds its own veto.
    crate::lid_awake::sync(app, &local);
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
    let _ = app.emit("sessions_updated", display_snapshot(app));
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

/// Push the current start-approval requests to the widget.
///
/// Its own event rather than a field on the session snapshot: a pending request
/// is not a session — it has no status, no history and no row of its own — and
/// folding it into `sessions_updated` would make every state transition carry
/// it, and every approval re-render the whole list.
pub fn emit_start_approvals(app: &AppHandle) {
    let pending = app
        .try_state::<crate::start_approval::ApprovalQueue>()
        .map(|q| q.list())
        .unwrap_or_default();
    let _ = app.emit("start_approvals_updated", pending);
}

/// The requests awaiting an answer, for the widget's mount snapshot.
#[tauri::command]
pub fn get_start_approvals(app: AppHandle) -> Vec<crate::start_approval::PendingStart> {
    app.try_state::<crate::start_approval::ApprovalQueue>().map(|q| q.list()).unwrap_or_default()
}

/// Approve starting `request_id`'s project at `dir` on the peer that asked.
///
/// The grant hop happens **here**, not in the waiting HTTP handler, so that
/// approving a request whose caller has already given up does exactly the same
/// thing as approving one still on the line: the peer records the permission
/// either way, and the only difference is whether there is still someone to
/// deliver the original message. That is the whole point of letting a request
/// outlive its caller.
///
/// The click is not the authority — the peer's acceptance is. It re-checks the
/// directory against its own disk and its own Claude Code trust state, and a
/// refusal there leaves the request on the list rather than pretending it
/// worked.
#[tauri::command]
pub async fn approve_start(request_id: String, dir: String, app: AppHandle) -> Result<(), String> {
    let queue = app.try_state::<crate::start_approval::ApprovalQueue>().ok_or("the dashboard is still starting")?;
    let pending = queue.get(&request_id).ok_or("that request is no longer waiting")?;

    // The user may only approve a directory the peer itself offered. A free-text
    // path would be this machine asserting something about the other one's disk,
    // which is the mistake the candidate list exists to avoid.
    if !pending.candidates.iter().any(|c| c.dir == dir) {
        return Err("that directory was not one of the ones offered".into());
    }

    let device_name = app.try_state::<ConfigState>().map(|c| c.snapshot().sync.device_name).unwrap_or_default();
    let origin_addr = app
        .try_state::<AppState>()
        .and_then(|s| s.remote.lock().unwrap().get(&pending.device).map(|d| d.origin_addr.clone()))
        .unwrap_or_default();
    if origin_addr.is_empty() {
        return Err(format!("no address for \"{}\" — it has not pushed to this device", pending.device));
    }

    let envelope = crate::sync::GrantEnvelope { origin_device: device_name, project: pending.project.clone(), dir: dir.clone() };
    let receipt = crate::sync::send_grant_hop(&app, &origin_addr, &envelope).await;
    if !receipt.granted {
        tracing::warn!(chat_id = %format!("{}/{}", pending.device, pending.project), decision = "peer_refused", device = %pending.device, reason = ?receipt.reason, "peer refused a grant its user approved here");
        return Err(receipt.detail.unwrap_or_else(|| receipt.reason.unwrap_or_else(|| "the other machine refused it".into())));
    }
    // Namespaced the way remote row ids already are: this line is written on
    // the sending machine about a project on another one, and a bare id would
    // collide with a local row of the same name — three projects are checked out
    // on both of these machines.
    tracing::info!(chat_id = %format!("{}/{}", pending.device, pending.project), decision = "peer_grant", device = %pending.device, dir = %dir, "approved starting a project on a peer");

    queue.resolve(&request_id, Some(dir));
    emit_start_approvals(&app);
    Ok(())
}

/// Decline a start request. The peer is told nothing — it already answered the
/// message, and there is no standing state over there to undo.
#[tauri::command]
pub fn dismiss_start(request_id: String, app: AppHandle) {
    if let Some(queue) = app.try_state::<crate::start_approval::ApprovalQueue>() {
        if let Some(p) = queue.resolve(&request_id, None) {
            tracing::info!(chat_id = %format!("{}/{}", p.device, p.project), decision = "peer_refused", device = %p.device, reason = "start_dismissed", "declined a start request");
        }
    }
    emit_start_approvals(&app);
}

/// Bring the main window to the front because something needs an answer.
///
/// Separate from `toggle_main`, which is the user's own show/hide and must stay
/// a toggle: a request arriving while the window is already up must not hide it.
/// Reuses `reveal` so the un-minimize handling stays in one place.
pub fn reveal_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        // Best-effort: a window that refuses to come forward is not a reason to
        // fail the request, and the prompt is still in the widget when the user
        // next opens it.
        let _ = reveal(&window);
    }
}

/// The stored dialog for `id`, for a history window with no live row to read.
///
/// This is the *conversation*, asked for by id — a different question from what
/// is running, and one that asserts no liveness at all. Without it the two shared
/// one answer, and every project's history became unreadable after a restart
/// until that session happened to act again, the data being complete on disk the
/// whole time.
///
/// It stayed the only thing a restart could recover for as long as the dashboard
/// could not tell a closed session from an idle one. `session_restore` can now,
/// so rows do come back — but only for the sessions two independent sources
/// vouch for, and only carrying a status one of them was holding for us. This
/// fallback is unchanged by that and still needed: the history window can be
/// opened on a chat_id with no row at all.
///
/// The live snapshot still wins when it has the row (see `HistoryApp`): a live
/// session's dialog is ahead of what has been persisted, because the watcher
/// upserts the assistant's reply into the row before it is written out. This is
/// the fallback, not the source.
#[tauri::command]
pub fn get_persisted_dialog(id: String, app: AppHandle) -> Vec<crate::state::DialogEntry> {
    app.try_state::<PromptHistoryStore>()
        .and_then(|store| store.get(&id))
        .map(|s| s.dialog)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DialogEntry, DialogRole};

    /// `name_counts` keyed by the row id, which is what the caller then reads
    /// through each row's label — the shape the assertions care about.
    fn shared(rows: &[AgentSession], per_row: &[(&str, usize)]) -> Vec<usize> {
        let per_row: HashMap<String, usize> = per_row.iter().map(|(id, n)| ((*id).to_string(), *n)).collect();
        let counts = name_counts(rows, &per_row);
        rows.iter().map(|s| counts.get(s.display_label()).copied().unwrap_or(0)).collect()
    }

    fn named_row(id: &str, display_name: Option<&str>) -> AgentSession {
        let mut s = row(id, Status::Done, 0, None);
        s.display_name = display_name.map(str::to_string);
        s
    }

    #[test]
    fn a_row_of_its_own_answers_to_its_name_once() {
        assert_eq!(shared(&[named_row("dash", None)], &[("dash", 1)]), vec![1]);
    }

    #[test]
    fn two_sessions_under_one_row_are_two_tabs_with_one_caption() {
        // A `--fork-session --resume` migration, or two projects whose directory
        // basenames match with `projects_root` unset.
        assert_eq!(shared(&[named_row("web", None)], &[("web", 2)]), vec![2]);
    }

    #[test]
    fn two_rows_renamed_alike_both_report_the_collision() {
        // The harm lands on whichever row is resolved second, so neither may be
        // left looking clean.
        let rows = [named_row("one", Some("web")), named_row("two", Some("web"))];
        assert_eq!(shared(&rows, &[("one", 1), ("two", 1)]), vec![2, 2]);
    }

    #[test]
    fn a_row_the_registry_knows_nothing_about_still_counts_itself() {
        // The floor, and the reason for it: `attention::resolve_row` refuses these
        // two rows whether or not a live session sits behind either, so a count
        // that summed bare session numbers would read `1` — silent — on exactly
        // the rows whose sensor had already given up.
        let rows = [named_row("one", Some("web")), named_row("two", Some("web"))];
        assert_eq!(shared(&rows, &[("one", 1)]), vec![2, 2], "the second row has no registry session");
        assert_eq!(shared(&rows, &[]), vec![2, 2], "and neither does the first");
    }

    #[test]
    fn distinct_names_never_report_sharing() {
        let rows = [named_row("dash", None), named_row("printlab", None), named_row("assistant", Some("bga-assistant"))];
        assert_eq!(shared(&rows, &[("dash", 1), ("printlab", 1), ("assistant", 1)]), vec![1, 1, 1]);
    }

    #[test]
    fn a_remote_row_neither_counts_nor_is_counted() {
        // Attention and tab captions are about this machine; a peer's row shares
        // nothing with a local one even when the names match.
        let mut remote = named_row("web", None);
        remote.origin = Some("chrome".into());
        remote.id = "chrome/web".into();
        let rows = [named_row("web", None), remote];
        assert_eq!(shared(&rows, &[("web", 1)])[0], 1);
    }

    fn row(id: &str, status: Status, state_entered_at: i64, attended_at: Option<i64>) -> AgentSession {
        let state = AppState::new();
        state.apply_set(
            crate::state::SetInput {
                id: id.into(),
                status,
                label: None,
                source: None,
                model: None,
                input_tokens: None,
                dialog_entry: None,
                waiting_backstop_armed: false,
            },
            state_entered_at,
            &[],
            None,
        );
        let mut s = state.snapshot().pop().expect("row");
        s.attended_at = attended_at;
        s
    }

    #[test]
    fn a_finished_row_reads_done_until_it_is_read_then_idle() {
        // The whole model in one test: DONE means "finished, and you haven't
        // looked". There is no second badge because there is nothing left to say.
        let mut rows = vec![row("a", Status::Done, 1_000, None)];
        apply_read_as_idle(&mut rows);
        assert_eq!(rows[0].status, Status::Done, "unread");

        let mut rows = vec![row("a", Status::Done, 1_000, Some(2_000))];
        apply_read_as_idle(&mut rows);
        assert_eq!(rows[0].status, Status::Idle, "read");
    }

    #[test]
    fn reading_a_row_does_not_restart_its_timer() {
        // `state_entered_at` is untouched, so the row keeps counting from when the
        // agent finished rather than from when it was read — the only one of
        // IDLE's several origins whose elapsed number is worth reading.
        let mut rows = vec![row("a", Status::Done, 1_000, Some(9_000))];
        apply_read_as_idle(&mut rows);
        assert_eq!(rows[0].state_entered_at, 1_000);
    }

    #[test]
    fn a_late_assistant_flush_undoes_the_read_with_no_extra_machinery() {
        // `Stop` settles the row Done *before* Claude Code writes the final reply.
        // A read landing in that gap must not swallow the answer — and doesn't,
        // because the flip is derived from `content_at` at every emit rather than
        // written into `AppState`.
        let mut r = row("a", Status::Done, 1_000, Some(2_000));
        r.dialog.push(DialogEntry { role: DialogRole::Assistant, text: "the answer".into(), timestamp: 3_000, status: Status::Done, task_start: false });
        let mut rows = vec![r];
        apply_read_as_idle(&mut rows);
        assert_eq!(rows[0].status, Status::Done, "the reply arrived after the read");
    }

    #[test]
    fn only_finished_local_rows_are_flipped() {
        // A remote row's attention is not this device's to judge, and no other
        // status means "finished" — flipping one would be inventing a transition.
        let mut remote = row("a", Status::Done, 1_000, Some(2_000));
        remote.origin = Some("chrome".into());
        let mut rows = vec![remote, row("b", Status::Blocked, 1_000, Some(2_000)), row("c", Status::Working, 1_000, Some(2_000))];
        apply_read_as_idle(&mut rows);
        assert_eq!(rows[0].status, Status::Done, "remote");
        assert_eq!(rows[1].status, Status::Blocked);
        assert_eq!(rows[2].status, Status::Working);
    }
}
