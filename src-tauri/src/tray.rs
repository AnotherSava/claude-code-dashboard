use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

use crate::commands::emit_config_updated;
use crate::config::{AutoResize, ConfigState, HistoryFontSize, TrayBadge};

const MENU_SHOW_HIDE: &str = "show_hide";
const MENU_ALWAYS_ON_TOP: &str = "always_on_top";
const MENU_SAVE_POSITION: &str = "save_position";
const MENU_TERMINAL_TITLES: &str = "terminal_titles";
const MENU_AUTOSTART_OFF: &str = "autostart_off";
const MENU_AUTOSTART_OPEN: &str = "autostart_open";
const MENU_AUTOSTART_TRAY: &str = "autostart_tray";
const MENU_AUTO_RESIZE_NONE: &str = "auto_resize_none";
const MENU_AUTO_RESIZE_UP: &str = "auto_resize_up";
const MENU_AUTO_RESIZE_DOWN: &str = "auto_resize_down";
const MENU_HIST_FONT_SMALLEST: &str = "hist_font_smallest";
const MENU_HIST_FONT_SMALL: &str = "hist_font_small";
const MENU_HIST_FONT_REGULAR: &str = "hist_font_regular";
const MENU_HIST_FONT_LARGE: &str = "hist_font_large";
const MENU_HIST_FONT_LARGEST: &str = "hist_font_largest";
const MENU_TRAY_BADGE_NONE: &str = "tray_badge_none";
const MENU_TRAY_BADGE_5H_LIGHT: &str = "tray_badge_5h_light";
const MENU_TRAY_BADGE_7D_LIGHT: &str = "tray_badge_7d_light";
const MENU_TRAY_BADGE_5H_NUM: &str = "tray_badge_5h_num";
const MENU_TRAY_BADGE_7D_NUM: &str = "tray_badge_7d_num";
const MENU_OPEN_DATA_DIR: &str = "open_data_dir";
const MENU_HELP_ABOUT: &str = "help_about";
const MENU_HELP_INSTRUCTIONS: &str = "help_instructions";
const MENU_QUIT: &str = "quit";

/// Tray menu item handles kept in managed state so menu handlers can update
/// check-marks after toggling the underlying setting.
pub struct TrayHandles {
    pub always_on_top: CheckMenuItem<Wry>,
    pub save_position: CheckMenuItem<Wry>,
    pub terminal_titles: CheckMenuItem<Wry>,
    pub autostart_off: CheckMenuItem<Wry>,
    pub autostart_open: CheckMenuItem<Wry>,
    pub autostart_tray: CheckMenuItem<Wry>,
    pub auto_resize_none: CheckMenuItem<Wry>,
    pub auto_resize_up: CheckMenuItem<Wry>,
    pub auto_resize_down: CheckMenuItem<Wry>,
    pub hist_font_smallest: CheckMenuItem<Wry>,
    pub hist_font_small: CheckMenuItem<Wry>,
    pub hist_font_regular: CheckMenuItem<Wry>,
    pub hist_font_large: CheckMenuItem<Wry>,
    pub hist_font_largest: CheckMenuItem<Wry>,
    pub tray_badge_none: CheckMenuItem<Wry>,
    pub tray_badge_5h_light: CheckMenuItem<Wry>,
    pub tray_badge_7d_light: CheckMenuItem<Wry>,
    pub tray_badge_5h_num: CheckMenuItem<Wry>,
    pub tray_badge_7d_num: CheckMenuItem<Wry>,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show_hide = MenuItem::with_id(app, MENU_SHOW_HIDE, "Show / Hide", true, None::<&str>)?;

    let (aot_initial, save_pos_initial, auto_resize_initial, term_titles_initial) = app
        .try_state::<ConfigState>()
        .map(|s| {
            let c = s.snapshot();
            (c.always_on_top, c.save_window_position, c.auto_resize, c.terminal_titles)
        })
        .unwrap_or((true, false, AutoResize::None, true));
    let always_on_top = CheckMenuItem::with_id(
        app, MENU_ALWAYS_ON_TOP, "Always on top", true, aot_initial, None::<&str>,
    )?;

    let save_position = CheckMenuItem::with_id(
        app, MENU_SAVE_POSITION, "Save position on exit", true, save_pos_initial, None::<&str>,
    )?;

    let terminal_titles = CheckMenuItem::with_id(
        app, MENU_TERMINAL_TITLES, "Color terminal tabs", true, term_titles_initial, None::<&str>,
    )?;

    // Three-way autostart mode derived from two sources: whether the OS
    // launch entry is enabled, and (when it is) the persisted `start_minimized`
    // bit. Off = not enabled; Open window = enabled + show; Open to tray =
    // enabled + minimized.
    let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
    let start_minimized_initial = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().start_minimized)
        .unwrap_or(false);
    let autostart_off = CheckMenuItem::with_id(
        app, MENU_AUTOSTART_OFF, "Off", true, !autostart_enabled, None::<&str>,
    )?;
    let autostart_open = CheckMenuItem::with_id(
        app, MENU_AUTOSTART_OPEN, "Open window", true,
        autostart_enabled && !start_minimized_initial, None::<&str>,
    )?;
    let autostart_tray = CheckMenuItem::with_id(
        app, MENU_AUTOSTART_TRAY, "Open to tray", true,
        autostart_enabled && start_minimized_initial, None::<&str>,
    )?;
    let autostart_submenu = SubmenuBuilder::new(app, "On system start")
        .items(&[&autostart_off, &autostart_open, &autostart_tray])
        .build()?;

    let auto_resize_none = CheckMenuItem::with_id(
        app, MENU_AUTO_RESIZE_NONE, "None", true,
        auto_resize_initial == AutoResize::None, None::<&str>,
    )?;
    let auto_resize_up = CheckMenuItem::with_id(
        app, MENU_AUTO_RESIZE_UP, "Up", true,
        auto_resize_initial == AutoResize::Up, None::<&str>,
    )?;
    let auto_resize_down = CheckMenuItem::with_id(
        app, MENU_AUTO_RESIZE_DOWN, "Down", true,
        auto_resize_initial == AutoResize::Down, None::<&str>,
    )?;
    let auto_resize_submenu = SubmenuBuilder::new(app, "Auto resize")
        .items(&[&auto_resize_none, &auto_resize_up, &auto_resize_down])
        .build()?;

    let hist_font_initial = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().history_font_size)
        .unwrap_or_default();
    let hist_font_smallest = CheckMenuItem::with_id(app, MENU_HIST_FONT_SMALLEST, "Smallest", true, hist_font_initial == HistoryFontSize::Smallest, None::<&str>)?;
    let hist_font_small = CheckMenuItem::with_id(app, MENU_HIST_FONT_SMALL, "Small", true, hist_font_initial == HistoryFontSize::Small, None::<&str>)?;
    let hist_font_regular = CheckMenuItem::with_id(app, MENU_HIST_FONT_REGULAR, "Regular", true, hist_font_initial == HistoryFontSize::Regular, None::<&str>)?;
    let hist_font_large = CheckMenuItem::with_id(app, MENU_HIST_FONT_LARGE, "Large", true, hist_font_initial == HistoryFontSize::Large, None::<&str>)?;
    let hist_font_largest = CheckMenuItem::with_id(app, MENU_HIST_FONT_LARGEST, "Largest", true, hist_font_initial == HistoryFontSize::Largest, None::<&str>)?;
    let hist_font_submenu = SubmenuBuilder::new(app, "History font size")
        .items(&[&hist_font_smallest, &hist_font_small, &hist_font_regular, &hist_font_large, &hist_font_largest])
        .build()?;

    let tray_badge_initial = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().tray_badge)
        .unwrap_or_default();
    let tray_badge_none = CheckMenuItem::with_id(app, MENU_TRAY_BADGE_NONE, "None", true, tray_badge_initial == TrayBadge::None, None::<&str>)?;
    let tray_badge_5h_light = CheckMenuItem::with_id(app, MENU_TRAY_BADGE_5H_LIGHT, "5-hour limit (lights)", true, tray_badge_initial == TrayBadge::FiveHourLight, None::<&str>)?;
    let tray_badge_7d_light = CheckMenuItem::with_id(app, MENU_TRAY_BADGE_7D_LIGHT, "7-day limit (lights)", true, tray_badge_initial == TrayBadge::SevenDayLight, None::<&str>)?;
    let tray_badge_5h_num = CheckMenuItem::with_id(app, MENU_TRAY_BADGE_5H_NUM, "5-hour limit (number)", true, tray_badge_initial == TrayBadge::FiveHourNumber, None::<&str>)?;
    let tray_badge_7d_num = CheckMenuItem::with_id(app, MENU_TRAY_BADGE_7D_NUM, "7-day limit (number)", true, tray_badge_initial == TrayBadge::SevenDayNumber, None::<&str>)?;
    let tray_badge_submenu = SubmenuBuilder::new(app, "Tray usage badge")
        .items(&[&tray_badge_none, &tray_badge_5h_light, &tray_badge_7d_light, &tray_badge_5h_num, &tray_badge_7d_num])
        .build()?;

    let open_data_dir = MenuItem::with_id(app, MENU_OPEN_DATA_DIR, "Open config/logs location", true, None::<&str>)?;
    let help_about = MenuItem::with_id(app, MENU_HELP_ABOUT, "About", true, None::<&str>)?;
    let help_instructions = MenuItem::with_id(app, MENU_HELP_INSTRUCTIONS, "Connect instructions", true, None::<&str>)?;
    let help_submenu = SubmenuBuilder::new(app, "Help")
        .items(&[&help_about, &help_instructions])
        .build()?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show_hide,
            &PredefinedMenuItem::separator(app)?,
            &always_on_top,
            &save_position,
            &terminal_titles,
            &autostart_submenu,
            &auto_resize_submenu,
            &hist_font_submenu,
            &tray_badge_submenu,
            &PredefinedMenuItem::separator(app)?,
            &open_data_dir,
            &PredefinedMenuItem::separator(app)?,
            &help_submenu,
            &quit,
        ],
    )?;

    app.manage(TrayHandles {
        always_on_top: always_on_top.clone(),
        save_position: save_position.clone(),
        terminal_titles: terminal_titles.clone(),
        autostart_off: autostart_off.clone(),
        autostart_open: autostart_open.clone(),
        autostart_tray: autostart_tray.clone(),
        auto_resize_none: auto_resize_none.clone(),
        auto_resize_up: auto_resize_up.clone(),
        auto_resize_down: auto_resize_down.clone(),
        hist_font_smallest: hist_font_smallest.clone(),
        hist_font_small: hist_font_small.clone(),
        hist_font_regular: hist_font_regular.clone(),
        hist_font_large: hist_font_large.clone(),
        hist_font_largest: hist_font_largest.clone(),
        tray_badge_none: tray_badge_none.clone(),
        tray_badge_5h_light: tray_badge_5h_light.clone(),
        tray_badge_7d_light: tray_badge_7d_light.clone(),
        tray_badge_5h_num: tray_badge_5h_num.clone(),
        tray_badge_7d_num: tray_badge_7d_num.clone(),
    });

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("window icon".into()))?;

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("Claude Code Dashboard")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu_event(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

fn handle_menu_event(app: &AppHandle, id: &str) {
    match id {
        MENU_SHOW_HIDE => toggle_window(app),
        MENU_ALWAYS_ON_TOP => toggle_always_on_top(app),
        MENU_SAVE_POSITION => toggle_save_position(app),
        MENU_TERMINAL_TITLES => toggle_terminal_titles(app),
        MENU_AUTOSTART_OFF => select_autostart_mode(app, AutostartMode::Off),
        MENU_AUTOSTART_OPEN => select_autostart_mode(app, AutostartMode::OpenWindow),
        MENU_AUTOSTART_TRAY => select_autostart_mode(app, AutostartMode::OpenToTray),
        MENU_AUTO_RESIZE_NONE => select_auto_resize_mode(app, AutoResize::None),
        MENU_AUTO_RESIZE_UP => select_auto_resize_mode(app, AutoResize::Up),
        MENU_AUTO_RESIZE_DOWN => select_auto_resize_mode(app, AutoResize::Down),
        MENU_HIST_FONT_SMALLEST => select_history_font_size(app, HistoryFontSize::Smallest),
        MENU_HIST_FONT_SMALL => select_history_font_size(app, HistoryFontSize::Small),
        MENU_HIST_FONT_REGULAR => select_history_font_size(app, HistoryFontSize::Regular),
        MENU_HIST_FONT_LARGE => select_history_font_size(app, HistoryFontSize::Large),
        MENU_HIST_FONT_LARGEST => select_history_font_size(app, HistoryFontSize::Largest),
        MENU_TRAY_BADGE_NONE => select_tray_badge(app, TrayBadge::None),
        MENU_TRAY_BADGE_5H_LIGHT => select_tray_badge(app, TrayBadge::FiveHourLight),
        MENU_TRAY_BADGE_7D_LIGHT => select_tray_badge(app, TrayBadge::SevenDayLight),
        MENU_TRAY_BADGE_5H_NUM => select_tray_badge(app, TrayBadge::FiveHourNumber),
        MENU_TRAY_BADGE_7D_NUM => select_tray_badge(app, TrayBadge::SevenDayNumber),
        MENU_OPEN_DATA_DIR => open_data_dir(app),
        MENU_HELP_ABOUT => show_about(app),
        MENU_HELP_INSTRUCTIONS => show_setup_instructions(app),
        MENU_QUIT => {
            tracing::info!("tray quit invoked");
            // `app.exit(0)` going through Tauri's exit pipeline can silently
            // no-op when called from a menu handler with multiple windows
            // registered (observed after adding the tooltip overlay window).
            // `std::process::exit` bypasses the runtime entirely — fine for a
            // tray widget where there's no graceful work to flush on quit.
            std::process::exit(0);
        }
        _ => {}
    }
}

fn toggle_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(true) {
        let _ = window.hide();
        // Carry the About modal with the dashboard — leaving it visible
        // after the tray hides main produces a stray floating window.
        if let Some(about) = app.get_webview_window("about") {
            let _ = about.hide();
        }
    } else {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn toggle_always_on_top(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let new_state = !window.is_always_on_top().unwrap_or(false);
    let _ = window.set_always_on_top(new_state);
    if let Some(state) = app.try_state::<ConfigState>() {
        state.with_mut(|c| c.always_on_top = new_state);
        let _ = state.save_to_disk();
    }
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.always_on_top.set_checked(new_state);
    }
    emit_config_updated(app);
}

fn toggle_save_position(app: &AppHandle) {
    let Some(state) = app.try_state::<ConfigState>() else {
        return;
    };
    let new_state = !state.snapshot().save_window_position;
    state.with_mut(|c| c.save_window_position = new_state);
    let _ = state.save_to_disk();
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.save_position.set_checked(new_state);
    }
    emit_config_updated(app);
}

fn toggle_terminal_titles(app: &AppHandle) {
    let Some(state) = app.try_state::<ConfigState>() else {
        return;
    };
    let new_state = !state.snapshot().terminal_titles;
    state.with_mut(|c| c.terminal_titles = new_state);
    let _ = state.save_to_disk();
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.terminal_titles.set_checked(new_state);
    }
    emit_config_updated(app);
    // Apply immediately: the sync inside re-pushes titles on enable and
    // blanks them on disable, instead of waiting for the next state change.
    crate::commands::emit_sessions_updated(app);
}

fn select_auto_resize_mode(app: &AppHandle, mode: AutoResize) {
    let Some(state) = app.try_state::<ConfigState>() else {
        return;
    };
    if state.snapshot().auto_resize == mode {
        // Re-clicking the active option: keep it checked, no work to do.
        sync_auto_resize_checks(app, mode);
        return;
    }
    state.with_mut(|c| c.auto_resize = mode);
    let _ = state.save_to_disk();
    sync_auto_resize_checks(app, mode);
    // For Up/Down, the frontend $effect drives the snap with a freshly
    // measured height. For None, the frontend early-returns and never
    // invokes apply, so we have to clear the prior mode's min/max
    // constraints from here or the window stays height-locked forever.
    if matches!(mode, AutoResize::None) {
        if let Some(window) = app.get_webview_window("main") {
            let _ = crate::auto_resize::apply(&window, AutoResize::None, 0.0);
        }
    }
    emit_config_updated(app);
}

fn sync_auto_resize_checks(app: &AppHandle, mode: AutoResize) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    let _ = handles.auto_resize_none.set_checked(mode == AutoResize::None);
    let _ = handles.auto_resize_up.set_checked(mode == AutoResize::Up);
    let _ = handles.auto_resize_down.set_checked(mode == AutoResize::Down);
}

fn select_history_font_size(app: &AppHandle, size: HistoryFontSize) {
    let Some(state) = app.try_state::<ConfigState>() else { return };
    if state.snapshot().history_font_size == size {
        sync_history_font_checks(app, size);
        return;
    }
    state.with_mut(|c| c.history_font_size = size);
    let _ = state.save_to_disk();
    sync_history_font_checks(app, size);
    emit_config_updated(app);
}

pub fn sync_history_font_checks(app: &AppHandle, size: HistoryFontSize) {
    let Some(handles) = app.try_state::<TrayHandles>() else { return };
    let _ = handles.hist_font_smallest.set_checked(size == HistoryFontSize::Smallest);
    let _ = handles.hist_font_small.set_checked(size == HistoryFontSize::Small);
    let _ = handles.hist_font_regular.set_checked(size == HistoryFontSize::Regular);
    let _ = handles.hist_font_large.set_checked(size == HistoryFontSize::Large);
    let _ = handles.hist_font_largest.set_checked(size == HistoryFontSize::Largest);
}

fn select_tray_badge(app: &AppHandle, mode: TrayBadge) {
    let Some(state) = app.try_state::<ConfigState>() else { return };
    if state.snapshot().tray_badge != mode {
        state.with_mut(|c| c.tray_badge = mode);
        let _ = state.save_to_disk();
        emit_config_updated(app);
    }
    sync_tray_badge_checks(app, mode);
    // Repaint the icon/tooltip immediately rather than waiting for the next
    // usage poll.
    crate::tray_badge::refresh(app);
}

fn sync_tray_badge_checks(app: &AppHandle, mode: TrayBadge) {
    let Some(handles) = app.try_state::<TrayHandles>() else { return };
    let _ = handles.tray_badge_none.set_checked(mode == TrayBadge::None);
    let _ = handles.tray_badge_5h_light.set_checked(mode == TrayBadge::FiveHourLight);
    let _ = handles.tray_badge_7d_light.set_checked(mode == TrayBadge::SevenDayLight);
    let _ = handles.tray_badge_5h_num.set_checked(mode == TrayBadge::FiveHourNumber);
    let _ = handles.tray_badge_7d_num.set_checked(mode == TrayBadge::SevenDayNumber);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutostartMode {
    Off,
    OpenWindow,
    OpenToTray,
}

/// Apply a chosen autostart mode: flip the OS launch entry on/off and persist
/// the `start_minimized` bit that distinguishes "Open window" from "Open to
/// tray". The minimized bit only takes effect at the *next* login launch (it's
/// read in `lib.rs` startup), so changing the mode while running never moves
/// the current window.
fn select_autostart_mode(app: &AppHandle, mode: AutostartMode) {
    let manager = app.autolaunch();
    let _ = match mode {
        AutostartMode::Off => manager.disable(),
        AutostartMode::OpenWindow | AutostartMode::OpenToTray => manager.enable(),
    };
    if let Some(state) = app.try_state::<ConfigState>() {
        let minimized = mode == AutostartMode::OpenToTray;
        if state.snapshot().start_minimized != minimized {
            state.with_mut(|c| c.start_minimized = minimized);
            let _ = state.save_to_disk();
        }
    }
    // Reconcile the radio checks against the now-authoritative OS state rather
    // than the requested mode — if enable/disable failed, the marks reflect
    // reality instead of an optimistic guess.
    sync_autostart_checks(app);
}

fn sync_autostart_checks(app: &AppHandle) {
    let Some(handles) = app.try_state::<TrayHandles>() else { return };
    let enabled = app.autolaunch().is_enabled().unwrap_or(false);
    let minimized = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().start_minimized)
        .unwrap_or(false);
    let _ = handles.autostart_off.set_checked(!enabled);
    let _ = handles.autostart_open.set_checked(enabled && !minimized);
    let _ = handles.autostart_tray.set_checked(enabled && minimized);
}

fn open_data_dir(app: &AppHandle) {
    let Ok(dir) = app.path().app_data_dir() else {
        tracing::warn!("open_data_dir: app_data_dir unavailable");
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = open::that(&dir) {
        tracing::warn!(?e, path = %dir.display(), "open_data_dir failed");
    }
}

/// Surface the onboarding panel on demand: bring the main window forward and
/// emit a frontend event so App.svelte forces SetupPanel visible, overriding
/// the `has_history` auto-hide.
fn show_setup_instructions(app: &AppHandle) {
    use tauri::Emitter;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
    let _ = app.emit("show_setup_instructions", ());
}

fn show_about(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("about") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}
