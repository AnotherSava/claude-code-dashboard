mod adapters;
mod auto_resize;
mod chat_id_registry;
mod commands;
mod config;
mod config_watcher;
mod custom_names;
mod http_server;
mod label_policy;
mod log_watcher;
mod logging;
mod notifications;
mod prompt_history;
mod setup;
mod state;
mod telegram;
mod tray;
mod usage_limits;

use config::ConfigState;
use log_watcher::WatcherRegistry;
use state::AppState;
use usage_limits::{UsageLimitsPoller, UsageLimitsState};

// Ties this crate's compilation to the frontend dist fingerprint computed in
// build.rs, so a frontend-only change re-embeds the UI on an incremental local
// build instead of shipping stale assets. See build.rs for the full rationale.
const _: &str = env!("FRONTEND_FINGERPRINT");

/// Tauri serves the frontend from a fixed `index.html` URL, and on Windows
/// WebView2 caches that response in its user-data folder (`EBWebView`). The
/// filename never changes, so a redeploy or app update that only swaps the
/// content-hashed JS/CSS bundle leaves WebView2 serving a *stale* cached
/// `index.html` that still points at the previous bundle — the UI then silently
/// runs old frontend code (e.g. an onboarding panel that predates its fix).
///
/// The staleness is sticky: it recurs on *every* launch (not just the first
/// after an update), survives `--disable-http-cache`, and a build-fingerprint
/// gate doesn't help because it happens *within* a single build. The only thing
/// that reliably forces a fresh fetch is deleting the cache. Since the frontend
/// is embedded in the binary (served from memory, no network), the WebView2
/// cache buys us nothing — so we wipe it on every startup, before the webview is
/// created. Windows-only: the macOS WKWebView custom-scheme handler doesn't
/// exhibit this staleness.
#[cfg(windows)]
fn clear_webview_cache() {
    let Ok(local) = std::env::var("LOCALAPPDATA") else { return };
    // Mirrors `identifier` in tauri.conf.json — Tauri derives the WebView2
    // user-data folder from it.
    let webview = std::path::Path::new(&local)
        .join("com.anothersava.claude-code-dashboard")
        .join("EBWebView");
    let _ = std::fs::remove_dir_all(webview);
}

#[cfg(not(windows))]
fn clear_webview_cache() {}

/// Appended to the autostart launch command (see the plugin init in `run`).
/// Its presence in the process args means this launch was triggered by the OS
/// at login rather than by the user — the gate for honoring "Open to tray".
pub const AUTOSTART_ARG: &str = "--autostarted";

/// True when this process was started by the OS autostart entry (i.e. the
/// `AUTOSTART_ARG` marker is present in the launch arguments).
fn launched_via_autostart() -> bool {
    std::env::args().any(|a| a == AUTOSTART_ARG)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Must run before the Builder creates the config-defined webviews, which
    // begin loading `index.html` immediately — clearing later (e.g. in setup())
    // would be too late to affect the initial navigation.
    clear_webview_cache();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // Tag the login-launch command so startup can tell an autostart
            // launch from a manual one — the "Open to tray" mode only hides
            // the window when the launch actually came from autostart.
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .manage(WatcherRegistry::new())
        .manage(UsageLimitsState::new())
        .manage(commands::HistoryTarget(std::sync::Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            commands::get_sessions,
            commands::get_config,
            commands::get_usage_limits,
            commands::refresh_usage_limits,
            commands::apply_auto_resize,
            commands::frontend_log,
            commands::hide_window,
            commands::show_window,
            commands::toggle_window,
            commands::quit_app,
            commands::remove_session,
            commands::open_history,
            commands::get_window_label,
            commands::get_history_target,
            commands::close_window,
            commands::hide_history,
            commands::set_history_font_size,
            commands::set_chat_name,
            commands::test_telegram_notification,
            commands::get_setup_state,
            commands::open_hook_script_location,
            commands::open_setup_docs,
            commands::open_docs_home,
            commands::get_about_info,
            commands::open_about,
            commands::set_window_size,
        ])
        .setup(|app| {
            use tauri::Manager;

            // Run as a macOS accessory: no Dock icon, no app menu bar — the
            // tray icon is the only entry point, mirroring Windows where
            // skipTaskbar hides the window from the taskbar / Alt-Tab.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let app_data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data).ok();

            let (log_guard, frontend_logger) = logging::init(&app_data);
            app.manage(log_guard);
            app.manage(frontend_logger);
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "widget starting");

            let history_store =
                prompt_history::PromptHistoryStore::new(app_data.join("prompt_history.json"));
            app.manage(history_store);

            // Drop the embedded Python hook next to config.json so users can
            // paste its path into ~/.claude/settings.json without cloning the
            // repo. Overwrites on every launch to track app updates.
            if let Err(e) = setup::write_hook_script(&app_data) {
                tracing::warn!(?e, "failed to write claude_hook.py to app data dir");
            }

            app.manage(chat_id_registry::ChatIdRegistry::new(
                app_data.join("session_chat_ids.json"),
            ));

            app.manage(custom_names::CustomNamesStore::new(
                app_data.join("custom_names.json"),
            ));

            let config_path = app_data.join("config.json");

            let config_state = ConfigState::new(config_path.clone());
            // Ensure a config.json exists on first run so external editing
            // works without further steps. The same first-run signal also
            // bootstraps autostart on by default — users can opt out via
            // the tray menu, and the choice lives in the OS (registry on
            // Windows, LaunchAgent on macOS), so re-enabling here would
            // fight the user on every launch.
            if !config_path.exists() {
                let _ = config_state.save_to_disk();
                use tauri_plugin_autostart::ManagerExt;
                let _ = app.autolaunch().enable();
            }
            let current_config = config_state.snapshot();
            let server_port = current_config.server_port;
            app.manage(config_state);

            // "Open to tray": when the OS launched us at login and the user
            // picked the minimized mode, keep the main window in the tray by
            // suppressing both automatic reveal paths (frontend mount-time
            // `show_window` and the safety-net timer below).
            let start_minimized = current_config.start_minimized && launched_via_autostart();
            app.manage(commands::SuppressInitialShow(
                std::sync::atomic::AtomicBool::new(start_minimized),
            ));

            // Apply config to the window
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_always_on_top(current_config.always_on_top);
                match (current_config.save_window_position, current_config.window_position) {
                    (true, Some(pos)) => {
                        let _ = window.set_position(tauri::PhysicalPosition::new(pos.x, pos.y));
                        // Restore size if a prior run captured it. Old configs
                        // (or never-resized fresh installs) leave w/h as None
                        // and we keep the conf-default geometry.
                        if let (Some(w), Some(h)) = (pos.width, pos.height) {
                            let _ = window.set_size(tauri::PhysicalSize::new(w, h));
                        }
                    }
                    _ => {
                        config_watcher::apply_default_position(&window);
                    }
                }
                // Install the WM_NCHITTEST + WM_NCLBUTTONDOWN subclass.
                // Lock is inactive until apply() flips it on, so this is a
                // no-op until the user picks an Up/Down mode.
                auto_resize::install_resize_lock(&window);
                // Force the window class's background brush to the dark
                // theme color, so growing the window via left/right resize
                // doesn't paint a brief flash of white before the webview
                // renders into the new area.
                auto_resize::set_dark_window_background(&window);

                // Safety net: if the frontend never calls `show_window`
                // (broken JS, slow webview), reveal the window anyway — unless
                // we started minimized to tray, where staying hidden is the
                // whole point.
                if !start_minimized {
                    let window_for_timeout = window.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        if matches!(window_for_timeout.is_visible(), Ok(false)) {
                            let _ = window_for_timeout.show();
                        }
                    });
                }
            }

            tray::setup(app.handle())?;
            config_watcher::spawn(app.handle().clone(), config_path);
            notifications::NotificationManager::spawn(app.handle().clone());
            UsageLimitsPoller::spawn(app.handle().clone());

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                http_server::run(handle, server_port).await;
            });

            #[cfg(debug_assertions)]
            seed_dev_sessions(&app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                match window.label() {
                    "main" => save_window_position_if_enabled(window),
                    "history" => {
                        use tauri::Emitter;
                        api.prevent_close();
                        save_history_position_if_enabled(window);
                        let _ = window.hide();
                        let _ = window.emit("history_hidden", ());
                    }
                    "about" => {
                        // About is informational — keep it alive across opens
                        // so we don't pay the webview cold-start each time the
                        // user picks Help → About.
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    _ => {}
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn save_history_position_if_enabled(window: &tauri::Window) {
    use tauri::Manager;
    let Some(state) = window.try_state::<ConfigState>() else { return };
    if !state.snapshot().save_window_position { return }
    let Ok(pos) = window.outer_position() else { return };
    let size = window.outer_size().ok();
    state.with_mut(|c| {
        c.history_window_position = Some(config::WindowPosition {
            x: pos.x,
            y: pos.y,
            width: size.map(|s| s.width),
            height: size.map(|s| s.height),
        })
    });
    let _ = state.save_to_disk();
}

fn save_window_position_if_enabled(window: &tauri::Window) {
    use tauri::Manager;
    let Some(state) = window.try_state::<ConfigState>() else {
        return;
    };
    let should_save = state.snapshot().save_window_position;
    if !should_save {
        return;
    }
    let Ok(pos) = window.outer_position() else {
        return;
    };
    let size = window.outer_size().ok();
    state.with_mut(|c| {
        c.window_position = Some(config::WindowPosition {
            x: pos.x,
            y: pos.y,
            width: size.map(|s| s.width),
            height: size.map(|s| s.height),
        });
    });
    let _ = state.save_to_disk();
}

#[cfg(debug_assertions)]
fn seed_dev_sessions(app: &tauri::AppHandle) {
    use crate::commands::{emit_sessions_updated, now_ms};
    use crate::state::{SetInput, Status};
    use tauri::Manager;

    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let now = now_ms();
    let s = 1000;
    let min = 60 * s;

    state.apply_set(
        SetInput {
            id: "claude-code-dashboard".into(),
            status: Status::Working,
            label: Some("I want to migrate an existing electron project to tauri framework".into()),
            source: Some("claude-code".into()),
            model: Some("claude-opus-4-7".into()),
            input_tokens: Some(75_000),
            dialog_entry: None,
        },
        now - 3 * min,
        &[],
        None,
    );

    state.apply_set(
        SetInput {
            id: "auth-service".into(),
            status: Status::Working,
            label: Some("Add pytest coverage for auth module".into()),
            source: Some("claude-code".into()),
            model: Some("claude-sonnet-4-6".into()),
            input_tokens: Some(152_000),
            dialog_entry: None,
        },
        now - 4 * min - 12 * s,
        &[],
        None,
    );
    state.apply_set(
        SetInput {
            id: "auth-service".into(),
            status: Status::Awaiting,
            label: Some("Can I run bash: pytest -xvs tests/test_auth.py?".into()),
            source: None,
            model: None,
            input_tokens: Some(152_000),
            dialog_entry: None,
        },
        now - 45 * s,
        &[],
        None,
    );

    emit_sessions_updated(app);
}
