use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::config::AutoResize;

/// Resize the window to a physical inner height of `desired_height_phys` while
/// preserving the edge anchored by `mode`. The height arrives already in
/// physical pixels (the frontend multiplies its CSS-pixel measurement by the
/// webview's own `devicePixelRatio`) so we never route it through
/// `window.scale_factor()`: near a mixed-DPI monitor boundary Rust's
/// scale_factor and the webview's devicePixelRatio disagree, and applying a
/// logical height under Rust's scale lands the viewport at the wrong size —
/// permanent false-overflow, a content scrollbar, and a re-triggering measure
/// loop that drifts the window across monitors. Vertical drag is prevented by
/// the WM_NCHITTEST subclass installed at startup (Windows-only).
pub fn apply(
    window: &WebviewWindow,
    mode: AutoResize,
    desired_height_phys: f64,
) -> tauri::Result<()> {
    if matches!(mode, AutoResize::None) {
        nchittest::set_active(false);
        return Ok(());
    }

    let pos = window.outer_position()?;
    // Use inner_size: set_size writes to the inner (client) area; reading
    // outer would give an inflated value on frameless Windows windows
    // because of the invisible resize border.
    let size = window.inner_size()?;
    let new_height_phys = desired_height_phys.round() as i32;
    let current_height_phys = size.height as i32;

    let raw_y = match mode {
        AutoResize::Up => pos.y + (current_height_phys - new_height_phys),
        AutoResize::Down => pos.y,
        AutoResize::None => unreachable!(),
    };

    // Clamp to the current monitor's work area (the region not covered by
    // the macOS Dock/menu bar or the Windows taskbar) on all four sides, so
    // the resize can never leave the window partially off-screen. Without
    // the bottom/right clamps, a window placed near a screen edge on first
    // launch — or one whose saved position was on a now-disconnected
    // monitor — would resize and stay where it was, with content cut off.
    let bounds = window
        .current_monitor()?
        .as_ref()
        .map(WorkAreaBounds::from_monitor)
        .unwrap_or_else(WorkAreaBounds::unbounded);
    let width_phys = size.width as i32;
    let (new_x, new_y) = bounds.clamp(pos.x, raw_y, width_phys, new_height_phys);

    window.set_size(PhysicalSize::new(size.width, new_height_phys.max(1) as u32))?;
    // Only reposition when the target actually differs from where the window
    // already sits. `set_size` keeps the top-left fixed, so an in-bounds
    // Down-mode resize (and any resize the clamp doesn't touch) needs no move.
    // Calling set_position unconditionally re-introduced a per-call drift: on a
    // frameless window the outer_position round-trip is inconsistent across a
    // DPI boundary, so each call nudged the window and a re-triggering measure
    // loop marched it across monitors.
    if new_x != pos.x || new_y != pos.y {
        window.set_position(PhysicalPosition::new(new_x, new_y))?;
    }
    nchittest::set_active(true);
    // `scale` is logged, not used for sizing — it's the value that disagrees
    // with the webview's devicePixelRatio across a DPI boundary, so capturing
    // both sides confirms the mismatch from the trace.
    let scale = window.scale_factor().unwrap_or(0.0);
    tracing::debug!(
        ?mode,
        desired_height_phys,
        new_height_phys,
        scale,
        new_x,
        new_y,
        "auto_resize::apply"
    );
    Ok(())
}

/// Physical-pixel bounds of a monitor's work area, with a clamp helper that
/// keeps a (x, y, w, h) rect inside it. Extracted so the clamping logic is
/// testable without a real `Monitor` and reusable across `auto_resize::apply`,
/// `config_watcher::apply_default_position`, and `commands::set_window_size`.
#[derive(Clone, Copy)]
pub(crate) struct WorkAreaBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl WorkAreaBounds {
    pub(crate) fn from_monitor(m: &tauri::Monitor) -> Self {
        let w = m.work_area();
        Self {
            left: w.position.x,
            top: w.position.y,
            right: w.position.x + w.size.width as i32,
            bottom: w.position.y + w.size.height as i32,
        }
    }

    fn unbounded() -> Self {
        Self { left: i32::MIN / 2, top: i32::MIN / 2, right: i32::MAX / 2, bottom: i32::MAX / 2 }
    }

    /// Physical-px overlap between this work area and the rect `(x, y, w, h)`
    /// on the horizontal axis; 0 when they don't overlap.
    pub(crate) fn overlap_x(&self, x: i32, w: i32) -> i32 {
        ((x + w).min(self.right) - x.max(self.left)).max(0)
    }

    /// Vertical-axis counterpart of [`WorkAreaBounds::overlap_x`].
    pub(crate) fn overlap_y(&self, y: i32, h: i32) -> i32 {
        ((y + h).min(self.bottom) - y.max(self.top)).max(0)
    }

    /// Area (physical px²) of the intersection between this work area and the
    /// rect `(x, y, w, h)`; 0 when they don't overlap. `i64` so stacked-4K
    /// virtual desktops can't overflow the multiply.
    pub(crate) fn intersection_area(&self, x: i32, y: i32, w: i32, h: i32) -> i64 {
        self.overlap_x(x, w) as i64 * self.overlap_y(y, h) as i64
    }

    /// Clamp a top-left position so the rect `(x, y, w, h)` lies within the
    /// work area. If the rect is bigger than the work area on an axis, the
    /// top-left wins (better to show the top of an overlong widget than to
    /// leave it floating below the bottom edge).
    pub(crate) fn clamp(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
        let max_x = (self.right - w).max(self.left);
        let max_y = (self.bottom - h).max(self.top);
        let cx = x.clamp(self.left, max_x);
        let cy = y.clamp(self.top, max_y);
        (cx, cy)
    }
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

#[cfg(test)]
mod tests {
    use super::WorkAreaBounds;

    fn screen_1440_900() -> WorkAreaBounds {
        // 1440x900 logical at scale 2 with a 25 pt macOS menu bar (50 physical).
        WorkAreaBounds { left: 0, top: 50, right: 2880, bottom: 1800 }
    }

    #[test]
    fn clamp_keeps_in_bounds_rect_untouched() {
        let b = screen_1440_900();
        assert_eq!(b.clamp(500, 500, 840, 1200), (500, 500));
    }

    #[test]
    fn clamp_pulls_bottom_overflow_back_onto_screen() {
        // Simulates Up-mode resize where the window's bottom was already
        // off-screen because the first-launch position was placed below the
        // work area — the resize must move the window up so the bottom sits
        // on the work-area floor.
        let b = screen_1440_900();
        let (x, y) = b.clamp(2024, 1500, 840, 600);
        assert_eq!(y, 1200, "y clamped to bottom - height");
        assert_eq!(x, 2024, "x left alone when in bounds");
    }

    #[test]
    fn clamp_pulls_right_overflow_back_onto_screen() {
        let b = screen_1440_900();
        let (x, _) = b.clamp(2500, 500, 840, 600);
        assert_eq!(x, 2040, "x clamped to right - width");
    }

    #[test]
    fn clamp_respects_macos_menu_bar_top() {
        let b = screen_1440_900();
        let (_, y) = b.clamp(500, 10, 840, 600);
        assert_eq!(y, 50, "y clamped to work-area top, not monitor top");
    }

    #[test]
    fn clamp_oversized_rect_pins_to_top_left() {
        // When the rect is bigger than the work area, the .max() in the
        // bound clamp keeps the top-left anchored — showing the top of the
        // widget is more useful than showing nothing.
        let b = screen_1440_900();
        assert_eq!(b.clamp(100, 100, 4000, 4000), (0, 50));
    }

    #[test]
    fn clamp_unbounded_is_a_noop() {
        let b = WorkAreaBounds::unbounded();
        assert_eq!(b.clamp(12_345, -678, 840, 600), (12_345, -678));
    }

    #[test]
    fn overlap_zero_when_rect_fully_outside() {
        // Window stranded to the right of a now-disconnected external monitor.
        let b = screen_1440_900();
        assert_eq!(b.overlap_x(3000, 840), 0);
        assert_eq!(b.intersection_area(3000, 500, 840, 600), 0);
    }

    #[test]
    fn overlap_partial_sliver_is_small() {
        // Only 40px of an 840px-wide window pokes onto the left edge.
        let b = screen_1440_900();
        assert_eq!(b.overlap_x(-800, 840), 40);
    }

    #[test]
    fn intersection_area_counts_visible_patch() {
        let b = screen_1440_900();
        // Fully inside: area is the whole rect.
        assert_eq!(b.intersection_area(500, 500, 840, 600), 840 * 600);
    }
}
