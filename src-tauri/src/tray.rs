use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

use crate::commands::emit_config_updated;
use crate::config::{AutoResize, ConfigState, HistoryFontSize, LidAwakeMode, TrayBadge};

const MENU_SHOW_HIDE: &str = "show_hide";
const MENU_ALWAYS_ON_TOP: &str = "always_on_top";
const MENU_SAVE_POSITION: &str = "save_position";
const MENU_TERMINAL_TITLES: &str = "terminal_titles";
const MENU_COMPACT_MODE: &str = "compact_mode";
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
const MENU_LID_AWAKE_OFF: &str = "lid_awake_off";
const MENU_LID_AWAKE_START_NOW: &str = "lid_awake_start_now";
const MENU_LID_AWAKE_ON_BATTERY: &str = "lid_awake_on_battery";
const MENU_LID_AWAKE_ALWAYS: &str = "lid_awake_always";
const MENU_CONTEXT_ALERT: &str = "context_alert";
const MENU_HIGH_ALERT: &str = "high_alert";
const MENU_OPEN_DATA_DIR: &str = "open_data_dir";
const MENU_OPEN_INTENSITY: &str = "open_intensity";
const MENU_HELP_ABOUT: &str = "help_about";
const MENU_HELP_INSTRUCTIONS: &str = "help_instructions";
const MENU_QUIT: &str = "quit";

/// Submenu title for the lid-closed veto. The hold window governs every mode
/// equally, so it lives here rather than on one item — carried by a single item
/// it read as if only that one were time-limited.
fn lid_awake_title(minutes: u64) -> String {
    format!("Keep awake with lid closed ({minutes} min)")
}

/// Refresh that title after a config reload — `lid_awake_minutes` is only
/// editable in `config.json`, so without this the menu would keep advertising
/// the window length the app started with.
pub fn sync_lid_awake_title(app: &AppHandle, minutes: u64) {
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.lid_awake_submenu.set_text(lid_awake_title(minutes));
    }
}

/// Tray menu item handles kept in managed state so menu handlers can update
/// check-marks after toggling the underlying setting.
pub struct TrayHandles {
    pub always_on_top: CheckMenuItem<Wry>,
    pub save_position: CheckMenuItem<Wry>,
    pub terminal_titles: CheckMenuItem<Wry>,
    pub compact_mode: CheckMenuItem<Wry>,
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
    pub context_alert: CheckMenuItem<Wry>,
    pub high_alert: CheckMenuItem<Wry>,
    pub lid_awake_off: CheckMenuItem<Wry>,
    pub lid_awake_on_battery: CheckMenuItem<Wry>,
    pub lid_awake_always: CheckMenuItem<Wry>,
    pub lid_awake_start_now: CheckMenuItem<Wry>,
    /// Last text pushed to `lid_awake_start_now`, so the 5s countdown refresh
    /// only touches the menu when the displayed value actually changes.
    pub lid_awake_label: std::sync::Mutex<String>,
    pub lid_awake_submenu: tauri::menu::Submenu<Wry>,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show_hide = MenuItem::with_id(app, MENU_SHOW_HIDE, "Show / Hide", true, None::<&str>)?;

    let (aot_initial, save_pos_initial, auto_resize_initial, term_titles_initial, compact_mode_initial) = app
        .try_state::<ConfigState>()
        .map(|s| {
            let c = s.snapshot();
            (c.always_on_top, c.save_window_position, c.auto_resize, c.terminal_titles, c.compact_mode)
        })
        .unwrap_or((true, false, AutoResize::None, true, false));
    let always_on_top = CheckMenuItem::with_id(
        app, MENU_ALWAYS_ON_TOP, "Always on top", true, aot_initial, None::<&str>,
    )?;

    let save_position = CheckMenuItem::with_id(
        app, MENU_SAVE_POSITION, "Save position on exit", true, save_pos_initial, None::<&str>,
    )?;

    let terminal_titles = CheckMenuItem::with_id(
        app, MENU_TERMINAL_TITLES, "Color terminal tabs", true, term_titles_initial, None::<&str>,
    )?;

    let compact_mode = CheckMenuItem::with_id(
        app, MENU_COMPACT_MODE, "Compact view", true, compact_mode_initial, None::<&str>,
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

    // The lid-closed veto is macOS-only (it drives `pmset disablesleep`), so the
    // items exist everywhere but the submenu is only hung in the menu on macOS —
    // a dead entry on Windows would just be a puzzle.
    let (lid_awake_mode_initial, lid_awake_minutes) = app
        .try_state::<ConfigState>()
        .map(|s| {
            let c = s.snapshot();
            (c.lid_awake_mode, c.lid_awake_minutes)
        })
        .unwrap_or((LidAwakeMode::Off, 15));
    let lid_awake_off = CheckMenuItem::with_id(app, MENU_LID_AWAKE_OFF, "Off", true, lid_awake_mode_initial == LidAwakeMode::Off, None::<&str>)?;
    // Checkable, and grouped with Off: while a hold is running the tick sits
    // here and the label counts down, so the menu always shows whether the Mac
    // is actually being held awake. When the window lapses the tick returns to
    // the standing policy on its own — the check can't outlive the hold.
    let lid_awake_start_now = CheckMenuItem::with_id(app, MENU_LID_AWAKE_START_NOW, "Start now", true, false, None::<&str>)?;
    let lid_awake_on_battery = CheckMenuItem::with_id(
        app, MENU_LID_AWAKE_ON_BATTERY, "On battery only", true, lid_awake_mode_initial == LidAwakeMode::OnBattery, None::<&str>,
    )?;
    let lid_awake_always = CheckMenuItem::with_id(
        app, MENU_LID_AWAKE_ALWAYS, "Always", true, lid_awake_mode_initial == LidAwakeMode::Always, None::<&str>,
    )?;
    // The window is a property of the feature, not of one mode — every mode
    // holds for the same span once the lid shuts — so it belongs in the submenu
    // title rather than on a single item, where it read as Manual-only.
    // Two groups: what's happening right now (nothing, or a hold you started),
    // then the standing policy that arms on its own. All four are one radio set
    // — the tick marks whichever is actually in effect.
    let lid_awake_submenu = SubmenuBuilder::new(app, lid_awake_title(lid_awake_minutes))
        .items(&[&lid_awake_off, &lid_awake_start_now])
        .separator()
        .items(&[&lid_awake_on_battery, &lid_awake_always])
        .build()?;

    let context_alert_initial = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().tray_context_alert_enabled)
        .unwrap_or(true);
    let context_alert = CheckMenuItem::with_id(
        app, MENU_CONTEXT_ALERT, "Show high context usage", true, context_alert_initial, None::<&str>,
    )?;

    let high_alert_initial = app
        .try_state::<ConfigState>()
        .map(|s| s.snapshot().high_alert)
        .unwrap_or(false);
    let high_alert = CheckMenuItem::with_id(
        app, MENU_HIGH_ALERT, "High alert", true, high_alert_initial, None::<&str>,
    )?;

    let open_data_dir = MenuItem::with_id(app, MENU_OPEN_DATA_DIR, "Open config/logs location", true, None::<&str>)?;
    let open_intensity = MenuItem::with_id(app, MENU_OPEN_INTENSITY, "Work intensity", true, None::<&str>)?;
    let help_about = MenuItem::with_id(app, MENU_HELP_ABOUT, "About", true, None::<&str>)?;
    let help_instructions = MenuItem::with_id(app, MENU_HELP_INSTRUCTIONS, "Connect instructions", true, None::<&str>)?;
    let help_submenu = SubmenuBuilder::new(app, "Help")
        .items(&[&help_about, &help_instructions])
        .build()?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;

    let sep_top = PredefinedMenuItem::separator(app)?;
    let sep_tools = PredefinedMenuItem::separator(app)?;
    let sep_help = PredefinedMenuItem::separator(app)?;
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = vec![
        &show_hide,
        &sep_top,
        &always_on_top,
        &save_position,
        &terminal_titles,
        &compact_mode,
        &autostart_submenu,
        &auto_resize_submenu,
        &hist_font_submenu,
        &tray_badge_submenu,
    ];
    if cfg!(target_os = "macos") {
        items.push(&lid_awake_submenu);
    }
    items.extend([
        &context_alert as &dyn tauri::menu::IsMenuItem<Wry>,
        &high_alert,
        &sep_tools,
        &open_intensity,
        &open_data_dir,
        &sep_help,
        &help_submenu,
        &quit,
    ]);
    let menu = Menu::with_items(app, &items)?;

    app.manage(TrayHandles {
        always_on_top: always_on_top.clone(),
        save_position: save_position.clone(),
        terminal_titles: terminal_titles.clone(),
        compact_mode: compact_mode.clone(),
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
        context_alert: context_alert.clone(),
        high_alert: high_alert.clone(),
        lid_awake_off: lid_awake_off.clone(),
        lid_awake_on_battery: lid_awake_on_battery.clone(),
        lid_awake_always: lid_awake_always.clone(),
        lid_awake_start_now: lid_awake_start_now.clone(),
        lid_awake_label: std::sync::Mutex::new("Start now".to_string()),
        lid_awake_submenu: lid_awake_submenu.clone(),
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
                // A Control+click reads as a plain left click here (see
                // `control_key_held`); honor the macOS secondary-click
                // convention by opening the tray's *native* menu — the same
                // one right-click and two-finger click show (status-item
                // highlight, anchored under the icon) — instead of toggling.
                // `show_menu` (tray-icon 0.22+) drives the identical
                // `performClick` path, so both gestures render the same menu.
                #[cfg(target_os = "macos")]
                if control_key_held() {
                    let _ = tray.with_inner_tray_icon(|inner| inner.show_menu());
                    return;
                }
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
        MENU_COMPACT_MODE => toggle_compact_mode(app),
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
        MENU_LID_AWAKE_OFF => select_lid_awake_mode(app, LidAwakeMode::Off),
        MENU_LID_AWAKE_START_NOW => start_lid_awake_now(app),
        MENU_LID_AWAKE_ON_BATTERY => select_lid_awake_mode(app, LidAwakeMode::OnBattery),
        MENU_LID_AWAKE_ALWAYS => select_lid_awake_mode(app, LidAwakeMode::Always),
        MENU_CONTEXT_ALERT => toggle_context_alert(app),
        MENU_HIGH_ALERT => toggle_high_alert(app),
        MENU_OPEN_DATA_DIR => open_data_dir(app),
        MENU_OPEN_INTENSITY => show_intensity(app),
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
        // Recover a window stranded off-screen by a monitor change while the
        // app was running — otherwise "show" reveals it where it can't be seen.
        crate::commands::ensure_window_on_screen(&window);
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Whether the Control key is currently held. On macOS, Control+click is the
/// system-wide "secondary click" that should open the tray menu like a
/// right-click / two-finger click — but the status item reports it as a plain
/// left click and drops the modifier (`tray-icon` exposes none), so we recover
/// the live modifier state ourselves. Queries CoreGraphics rather than pulling
/// in an Objective-C runtime, matching `idle.rs`'s FFI style; the bit is
/// `kCGEventFlagMaskControl` (== `NSEventModifierFlagControl`, both `1 << 18`).
#[cfg(target_os = "macos")]
fn control_key_held() -> bool {
    // CGEventSourceStateID::kCGEventSourceStateCombinedSessionState = 0.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }
    const CONTROL_MASK: u64 = 1 << 18;
    let flags = unsafe { CGEventSourceFlagsState(0) };
    flags & CONTROL_MASK != 0
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

fn toggle_compact_mode(app: &AppHandle) {
    let Some(state) = app.try_state::<ConfigState>() else {
        return;
    };
    let new_state = !state.snapshot().compact_mode;
    state.with_mut(|c| c.compact_mode = new_state);
    let _ = state.save_to_disk();
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.compact_mode.set_checked(new_state);
    }
    emit_config_updated(app);
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

/// Pick the standing lid-closed policy.
///
/// Any policy other than `Off` needs the one-time privileged setup (a pinned
/// sudoers grant plus the boot-reset daemon), so the first selection runs the
/// installer behind a single administrator prompt. If the user cancels that, the
/// policy reverts to `Off` rather than sitting enabled-but-inert.
///
/// Picking any policy also cancels an outstanding "Start now": the four items
/// are one radio set, so the tick has to follow the selection rather than stay
/// on a hold the user has just overridden. `Off` additionally drops a live veto
/// immediately — it suppresses thermal and low-battery sleep, so it can't wait
/// for the next idle.
fn select_lid_awake_mode(app: &AppHandle, mode: LidAwakeMode) {
    crate::lid_awake::cancel_manual(app);
    sync_lid_awake_state(app, mode, None);

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Revert rather than sit enabled-but-inert: without the grant every arm
        // would fail, so a checked policy would promise a hold it can't deliver.
        if mode != LidAwakeMode::Off && !ensure_lid_awake_installed().await {
            apply_lid_awake_mode(&handle, LidAwakeMode::Off);
            sync_lid_awake_state(&handle, LidAwakeMode::Off, None);
            return;
        }
        apply_lid_awake_mode(&handle, mode);
        if mode == LidAwakeMode::Off {
            crate::lid_awake::release_now(&handle, "turned off from the tray");
        }
    });
}

/// Run the one-time privileged setup if it hasn't been done yet, reporting
/// whether the grant ended up in place. Shared by both entry points — picking a
/// policy and "Start now" — which differ only in how they recover from a
/// decline, not in how they install.
async fn ensure_lid_awake_installed() -> bool {
    if crate::lid_awake::installed() {
        return true;
    }
    match crate::lid_awake::install().await {
        Ok(()) => {
            tracing::info!("lid-awake privileged setup installed");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "lid-awake setup did not complete");
            false
        }
    }
}

/// Start one hold right now, whatever the standing policy is — the "about to
/// pick the laptop up" button. Runs the one-time setup first if it hasn't been
/// done, since this may well be the user's first contact with the feature.
fn start_lid_awake_now(app: &AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Nothing to revert here — the standing policy is untouched, so a
        // declined setup just means no hold starts.
        if !ensure_lid_awake_installed().await {
            return;
        }
        crate::lid_awake::arm_manual(&handle);
    });
}

fn apply_lid_awake_mode(app: &AppHandle, mode: LidAwakeMode) {
    let Some(state) = app.try_state::<ConfigState>() else { return };
    if state.snapshot().lid_awake_mode != mode {
        state.with_mut(|c| c.lid_awake_mode = mode);
        let _ = state.save_to_disk();
        emit_config_updated(app);
    }
}

/// Label for the "Start now" item: the countdown while a hold is running, the
/// invitation otherwise. Rounds up, so a hold never reads "0 min left" while
/// it's still holding.
fn lid_awake_start_now_label(remaining_ms: Option<i64>) -> String {
    match remaining_ms {
        Some(ms) if ms >= 60_000 => format!("{} min left", ms / 60_000),
        Some(_) => "<1 min left".to_string(),
        None => "Start now".to_string(),
    }
}

/// Push the live state into the menu: the tick marks whatever is actually in
/// effect — a running manual hold, else the standing policy — and the "Start
/// now" label carries the countdown.
///
/// No-op updates are dropped: this runs on every 5s tick, and rewriting an
/// unchanged `NSMenuItem` title would churn the menu for nothing.
pub fn sync_lid_awake_state(app: &AppHandle, mode: LidAwakeMode, manual_remaining_ms: Option<i64>) {
    let Some(handles) = app.try_state::<TrayHandles>() else { return };
    let holding = manual_remaining_ms.is_some();
    let label = lid_awake_start_now_label(manual_remaining_ms);

    let mut last = handles.lid_awake_label.lock().unwrap();
    if *last != label {
        let _ = handles.lid_awake_start_now.set_text(&label);
        *last = label;
    }

    let _ = handles.lid_awake_start_now.set_checked(holding);
    // While a hold runs the tick belongs to it, not to the policy underneath —
    // otherwise two items would read as active at once.
    let _ = handles.lid_awake_off.set_checked(!holding && mode == LidAwakeMode::Off);
    let _ = handles.lid_awake_on_battery.set_checked(!holding && mode == LidAwakeMode::OnBattery);
    let _ = handles.lid_awake_always.set_checked(!holding && mode == LidAwakeMode::Always);
}

fn toggle_context_alert(app: &AppHandle) {
    let Some(state) = app.try_state::<ConfigState>() else { return };
    let new_state = !state.snapshot().tray_context_alert_enabled;
    state.with_mut(|c| c.tray_context_alert_enabled = new_state);
    let _ = state.save_to_disk();
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.context_alert.set_checked(new_state);
    }
    emit_config_updated(app);
    // Repaint the icon immediately so the alert appears/clears without waiting
    // for the next usage poll or state change.
    crate::tray_badge::refresh(app);
}

fn toggle_high_alert(app: &AppHandle) {
    let Some(state) = app.try_state::<ConfigState>() else { return };
    let new_state = !state.snapshot().high_alert;
    state.with_mut(|c| c.high_alert = new_state);
    let _ = state.save_to_disk();
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.high_alert.set_checked(new_state);
    }
    // The notification reconciler reads `high_alert` from the config snapshot
    // on its next tick, so persisting is enough — no immediate refresh needed
    // (unlike the tray badge / context alert, which repaint the icon).
    emit_config_updated(app);
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

fn show_intensity(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("intensity") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_now_label_counts_down_and_never_reads_zero() {
        assert_eq!(lid_awake_start_now_label(None), "Start now");
        assert_eq!(lid_awake_start_now_label(Some(15 * 60_000)), "15 min left");
        assert_eq!(lid_awake_start_now_label(Some(60_000)), "1 min left");
        // A hold that is still running must never advertise "0 min left" — the
        // Mac is genuinely still being held awake at that point.
        assert_eq!(lid_awake_start_now_label(Some(59_999)), "<1 min left");
        assert_eq!(lid_awake_start_now_label(Some(1)), "<1 min left");
    }

    #[test]
    fn submenu_title_carries_the_window_length() {
        // The window governs every mode, so it belongs in the title rather than
        // on one item, where it read as applying to that item alone.
        assert_eq!(lid_awake_title(15), "Keep awake with lid closed (15 min)");
        assert_eq!(lid_awake_title(30), "Keep awake with lid closed (30 min)");
    }
}
