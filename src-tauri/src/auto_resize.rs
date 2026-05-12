use tauri::{LogicalSize, PhysicalPosition, WebviewWindow};

use crate::config::AutoResize;

/// Resize the window to fit `desired_logical_height` while preserving the
/// edge anchored by `mode`. Vertical drag is prevented by the WM_NCHITTEST
/// subclass installed at startup (Windows-only) — when the lock is active,
/// the OS treats the top, bottom, and corner resize handles as client area
/// so the cursor never even shows the resize affordance there.
pub fn apply(
    window: &WebviewWindow,
    mode: AutoResize,
    desired_logical_height: f64,
) -> tauri::Result<()> {
    if matches!(mode, AutoResize::None) {
        nchittest::set_active(false);
        return Ok(());
    }

    let scale = window.scale_factor()?;
    let pos = window.outer_position()?;
    // Use inner_size: set_size writes to the inner (client) area; reading
    // outer would give an inflated value on frameless Windows windows
    // because of the invisible resize border.
    let size = window.inner_size()?;
    let new_height_phys = (desired_logical_height * scale).round() as i32;
    let current_height_phys = size.height as i32;

    let raw_y = match mode {
        AutoResize::Up => pos.y + (current_height_phys - new_height_phys),
        AutoResize::Down => pos.y,
        AutoResize::None => unreachable!(),
    };

    // Clamp y to the current monitor's top edge (multi-monitor safe —
    // a non-primary monitor can have a non-zero origin).
    let monitor_top = window
        .current_monitor()?
        .map(|m| m.position().y)
        .unwrap_or(0);
    let new_y = raw_y.max(monitor_top);

    let current_width_logical = size.width as f64 / scale;
    window.set_size(LogicalSize::new(
        current_width_logical,
        desired_logical_height,
    ))?;
    window.set_position(PhysicalPosition::new(pos.x, new_y))?;
    nchittest::set_active(true);
    tracing::debug!(
        ?mode,
        desired_logical_height,
        new_height_phys,
        new_y,
        "auto_resize::apply"
    );
    Ok(())
}

/// Install the WM_NCHITTEST subclass on the main window. Called once at
/// startup; the lock starts inactive and is toggled by `apply()` based on
/// the configured `AutoResize` mode.
#[cfg(windows)]
pub fn install_resize_lock(window: &WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else {
        tracing::warn!("install_resize_lock: hwnd unavailable");
        return;
    };
    nchittest::install(hwnd.0 as isize);
}

#[cfg(not(windows))]
pub fn install_resize_lock(_window: &WebviewWindow) {}

/// Replace the window class's background brush with our dark theme color so
/// the OS-managed paint during a horizontal resize uses `#1c1c1e` instead
/// of the default white. Without this, growing the window to the side
/// flashes white briefly before the webview catches up and renders content.
#[cfg(windows)]
pub fn set_dark_window_background(window: &WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else {
        tracing::warn!("set_dark_window_background: hwnd unavailable");
        return;
    };
    win_chrome::set_class_background(hwnd.0 as isize);
}

/// Set NSWindow.backgroundColor (and the WKWebView's underlay) to the dark
/// theme color so the very first frame after `show()` is dark instead of
/// flashing white. `tauri.conf.json`'s `backgroundColor` field is supposed
/// to do this but isn't applied early enough on macOS — the runtime call is
/// reliable.
#[cfg(target_os = "macos")]
pub fn set_dark_window_background(window: &WebviewWindow) {
    use tauri::window::Color;
    if let Err(e) = window.set_background_color(Some(Color(0x1c, 0x1c, 0x1e, 0xff))) {
        tracing::warn!(?e, "set_background_color failed on macOS");
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn set_dark_window_background(_window: &WebviewWindow) {}

#[cfg(windows)]
mod nchittest {
    use std::sync::atomic::{AtomicBool, Ordering};

    // Minimal Win32 surface needed for WM_NCHITTEST subclassing. Declared
    // by hand to avoid a `windows`/`windows-sys` dep — these signatures are
    // stable since Comctl32 v6.
    type Hwnd = isize;
    type Wparam = usize;
    type Lparam = isize;
    type Lresult = isize;
    type SubclassProc = Option<
        unsafe extern "system" fn(Hwnd, u32, Wparam, Lparam, usize, usize) -> Lresult,
    >;

    const WM_NCHITTEST: u32 = 0x0084;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const HTCLIENT: u32 = 1;
    const HTTOP: u32 = 12;
    const HTTOPLEFT: u32 = 13;
    const HTTOPRIGHT: u32 = 14;
    const HTBOTTOM: u32 = 15;
    const HTBOTTOMLEFT: u32 = 16;
    const HTBOTTOMRIGHT: u32 = 17;

    fn is_blocked_edge(ht: u32) -> bool {
        matches!(
            ht,
            HTTOP | HTBOTTOM | HTTOPLEFT | HTTOPRIGHT | HTBOTTOMLEFT | HTBOTTOMRIGHT
        )
    }

    #[link(name = "comctl32")]
    extern "system" {
        fn SetWindowSubclass(
            hwnd: Hwnd,
            callback: SubclassProc,
            id: usize,
            refdata: usize,
        ) -> i32;
        fn DefSubclassProc(hwnd: Hwnd, msg: u32, wp: Wparam, lp: Lparam) -> Lresult;
    }

    /// Arbitrary unique-per-window-class id for our subclass — must not
    /// collide with any other subclass on the same HWND. "ARES" is just a
    /// recognizable marker in a debugger.
    const SUBCLASS_ID: usize = 0x4152_4553;

    static LOCK_ACTIVE: AtomicBool = AtomicBool::new(false);
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    pub fn set_active(active: bool) {
        LOCK_ACTIVE.store(active, Ordering::Relaxed);
    }

    pub fn install(hwnd_raw: isize) {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        let ok = unsafe {
            SetWindowSubclass(hwnd_raw, Some(subclass_proc), SUBCLASS_ID, 0)
        };
        if ok == 0 {
            INSTALLED.store(false, Ordering::SeqCst);
            tracing::warn!("SetWindowSubclass failed for resize lock");
        } else {
            tracing::debug!("resize-lock subclass installed");
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: Hwnd,
        msg: u32,
        wp: Wparam,
        lp: Lparam,
        _id: usize,
        _data: usize,
    ) -> Lresult {
        if LOCK_ACTIVE.load(Ordering::Relaxed) {
            match msg {
                // Neutralize the hit-test so the OS treats top/bottom/corner
                // edges as client area. This actually sticks for the bottom
                // edge (no resize cursor flash). For the top edge wry calls
                // SetCursor() directly inside its own message handlers, so
                // the cursor still flashes ↕ there — accepted as a cosmetic
                // limitation. The resize drag itself is blocked by the
                // WM_NCLBUTTONDOWN handler below regardless.
                WM_NCHITTEST => {
                    let result = DefSubclassProc(hwnd, msg, wp, lp);
                    let ht = result as u32;
                    if is_blocked_edge(ht) {
                        return HTCLIENT as Lresult;
                    }
                    return result;
                }
                // The message that *starts* the resize drag — wp carries
                // the hit-test value. Consume it for top/bottom/corners so
                // the OS never enters the resize loop, even when wry's
                // later subclass kept the hit-test as HTTOP/HTBOTTOM.
                WM_NCLBUTTONDOWN => {
                    let ht = wp as u32;
                    if is_blocked_edge(ht) {
                        return 0;
                    }
                }
                _ => {}
            }
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

#[cfg(not(windows))]
mod nchittest {
    pub fn set_active(_active: bool) {}
}

#[cfg(windows)]
mod win_chrome {
    // COLORREF for #1c1c1e (R=0x1c, G=0x1c, B=0x1e). Encoding is
    // 0x00BBGGRR, so #1c1c1e becomes 0x001E1C1C. Must match the .widget
    // background in src/App.svelte and `backgroundColor` in tauri.conf.json
    // — if any of those change, update this too.
    const COLOR_DARK_BG: u32 = 0x001E_1C1C;
    const GCLP_HBRBACKGROUND: i32 = -10;

    type Hwnd = isize;

    #[link(name = "user32")]
    extern "system" {
        fn SetClassLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateSolidBrush(color: u32) -> isize;
    }

    pub fn set_class_background(hwnd_raw: isize) {
        let brush = unsafe { CreateSolidBrush(COLOR_DARK_BG) };
        if brush == 0 {
            tracing::warn!("CreateSolidBrush failed for dark background");
            return;
        }
        // We deliberately don't DeleteObject the previous brush returned
        // here: the original class background may be a system color value
        // (e.g. COLOR_WINDOW+1) rather than a real GDI handle, and feeding
        // that to DeleteObject is unsafe. The one-time leak is acceptable.
        unsafe { SetClassLongPtrW(hwnd_raw, GCLP_HBRBACKGROUND, brush) };
        tracing::debug!("class background brush set to dark theme");
    }
}
