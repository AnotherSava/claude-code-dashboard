use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::config::AutoResize;

/// Is the window currently minimized? Wrapped so the "can't tell" case has one
/// definition — treat it as not minimized, which leaves the pre-existing
/// behaviour intact rather than silently freezing the widget's height on a
/// platform whose answer we failed to read.
pub fn is_minimized(window: &WebviewWindow) -> bool {
    window.is_minimized().unwrap_or(false)
}

/// Last observed minimized state, so the `WindowEvent::Resized` stream can be
/// reduced to the single edge that matters. See [`resized_is_restore`].
static WAS_MINIMIZED: AtomicBool = AtomicBool::new(false);

/// Fold a `WindowEvent::Resized` into the minimized-state tracker and report
/// whether this event is the *restore* edge (minimized → not minimized).
///
/// Windows sends `WM_SIZE` for the minimize and again for the restore, and tao
/// emits `Resized` for every one of them — including the ordinary resizes we
/// cause ourselves. Only the restore edge may trigger a re-fit: re-fitting on
/// every `Resized` would loop, since the re-fit resizes the window, which emits
/// another `Resized`.
///
/// Windows-only in practice, and that is fine rather than a gap: tao registers
/// no `windowDidMiniaturize:`/`windowDidDeminiaturize:`, and AppKit doesn't
/// resize a window to miniaturize it (the miniwindow is a separate entity), so
/// no `Resized` fires on either macOS edge and this never returns true there.
/// Nothing is missing, because a miniaturized `NSWindow` keeps its real frame —
/// macOS never corrupts the geometry, so there is nothing to recover from.
///
/// The state is one process-global bit while the signature takes a window. That
/// holds only because the sole caller resolves `main` itself; adding a second
/// window here would interleave one window's minimize with another's restore.
pub fn resized_is_restore(window: &WebviewWindow) -> bool {
    let now = is_minimized(window);
    let was = WAS_MINIMIZED.swap(now, Ordering::SeqCst);
    was && !now
}

/// Resize the window to a physical inner height of `desired_height_phys` while
/// preserving the edge anchored by `mode`. The height arrives already in
/// physical pixels (the frontend multiplies its CSS-pixel measurement by the
/// webview's own `devicePixelRatio`) so we never route it through
/// `window.scale_factor()`: near a mixed-DPI monitor boundary Rust's
/// scale_factor and the webview's devicePixelRatio disagree, and applying a
/// logical height under Rust's scale lands the viewport at the wrong size —
/// permanent false-overflow, a content scrollbar, and a re-triggering measure
/// loop that drifts the window across monitors. Manual vertical drag is
/// prevented for the height we apply here by [`resize_lock`].
pub fn apply(
    window: &WebviewWindow,
    mode: AutoResize,
    desired_height_phys: f64,
) -> tauri::Result<()> {
    if matches!(mode, AutoResize::None) {
        resize_lock::release(window);
        return Ok(());
    }

    // A minimized window does not report its own geometry. Windows substitutes
    // the iconic rect: `outer_position` reads (-32000, -32000) and `inner_size`
    // reads SM_CXMINIMIZED x SM_CYMINIMIZED (237x39 at 144 DPI). Every input
    // below is then meaningless — the anchor arithmetic, the work-area clamp
    // (which always resolves to a corner, since the iconic rect overlaps no
    // monitor) and the width we carry over — and the resize lands on the real
    // window when it comes back. That is how a full-screen game, which
    // minimizes the always-on-top widget, left it collapsed to a header sliver.
    //
    // Deliberately keyed on minimized and NOT on visibility: a window hidden to
    // the tray keeps a real rect, and skipping there would bring the widget back
    // stale on every tray toggle. `lib.rs` re-fits on the restore edge, when the
    // geometry means something again.
    if is_minimized(window) {
        tracing::debug!(desired_height_phys, reason = "minimized", "auto_resize::apply skipped");
        return Ok(());
    }

    let pos = window.outer_position()?;
    // Use inner_size: set_size writes to the inner (client) area; reading
    // outer would give an inflated value on frameless Windows windows
    // because of the invisible resize border.
    let size = window.inner_size()?;
    // Clamp to a positive height up front so the anchor math, the resize and
    // the lock's height pin all agree on one value.
    let new_height_phys = (desired_height_phys.round() as i32).max(1);
    let current_height_phys = size.height as i32;

    let raw_y = match mode {
        AutoResize::Up => pos.y + (current_height_phys - new_height_phys),
        AutoResize::Down => pos.y,
        AutoResize::None => unreachable!(),
    };

    // Clamp the resize to a monitor's work area (the region not covered by
    // the macOS Dock/menu bar or the Windows taskbar) on all four sides, so
    // the resize can never leave the window partially off-screen. Without
    // the bottom/right clamps, a window placed near a screen edge on first
    // launch — or one whose saved position was on a now-disconnected
    // monitor — would resize and stay where it was, with content cut off.
    // (Which monitor's work area, and why not `current_monitor()`, below.)
    let width_phys = size.width as i32;
    // Clamp against the monitor the window actually occupies — the one its rect
    // overlaps most — rather than `current_monitor()`. Near a mixed-DPI boundary
    // `current_monitor()` misreports which display holds the window, and clamping
    // the real position against the *other* monitor's work area teleported the
    // window to a corner (observed: repeated jumps to (0,0) on the primary,
    // dropping the widget on top of the foreground app — see
    // debug_auto_resize_dpi_drift). Overlap selection leaves an on-screen window
    // untouched; a window stranded off every monitor falls back to the primary
    // so the clamp can still rescue it back onto the screen.
    let bounds = clamp_bounds(window, pos.x, raw_y, width_phys, new_height_phys);
    let (new_x, new_y) = bounds.clamp(pos.x, raw_y, width_phys, new_height_phys);

    // Re-arm the lock *before* the resize, not after. On macOS the lock is a
    // min/max content-height pin, so a pin left over from the previous apply()
    // would clamp this `set_size` back to the old height. Engaging first is
    // also the only ordering that stays correct whichever way the platform
    // applies the resize (macOS defers it to the main queue, Windows doesn't).
    resize_lock::engage(window, new_height_phys);
    window.set_size(PhysicalSize::new(size.width, new_height_phys as u32))?;
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
    // `scale` is logged, not used for sizing — it's the value that disagrees
    // with the webview's devicePixelRatio across a DPI boundary, so capturing
    // both sides confirms the mismatch from the trace.
    let scale = window.scale_factor().unwrap_or(0.0);
    // Ground-truth geometry for the mixed-DPI drift class: `os_dpi` is the fresh
    // per-monitor DPI (GetDpiForWindow), which — unlike `scale` (Tao's cached
    // scale_factor) — cannot go stale, and `actual_client_h` is the real client
    // height read back *after* set_size. `actual_client_h != new_height_phys`
    // means the OS swallowed or rescaled the resize; `os_dpi/96 != scale` means
    // Tao's scale went stale. Both are the signatures the auto-resize DPI notes
    // chase, now answerable straight from the trace.
    let (actual_client_h, os_dpi) = geom::actual_client_and_dpi(window).unwrap_or((-1, 0));
    tracing::debug!(
        ?mode,
        desired_height_phys,
        new_height_phys,
        scale,
        os_dpi,
        actual_client_h,
        new_x,
        new_y,
        "auto_resize::apply"
    );
    Ok(())
}

/// Pick the work-area bounds `auto_resize::apply` clamps against: the monitor
/// the rect `(x, y, w, h)` overlaps most. When the rect overlaps no monitor
/// (stranded on a since-disconnected display) fall back to the primary so the
/// clamp still pulls it back on-screen; if even that is unavailable, stay
/// unbounded (no clamp). Deliberately avoids `current_monitor()`, which
/// misreports the holding monitor near a mixed-DPI boundary and drove the
/// (0,0) teleport (see the `debug_auto_resize_dpi_drift` note).
fn clamp_bounds(window: &WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> WorkAreaBounds {
    let areas: Vec<WorkAreaBounds> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(WorkAreaBounds::from_monitor)
        .collect();
    if let Some(bounds) = WorkAreaBounds::best_overlap(&areas, x, y, w, h) {
        return bounds;
    }
    window
        .primary_monitor()
        .ok()
        .flatten()
        .as_ref()
        .map(WorkAreaBounds::from_monitor)
        .unwrap_or_else(WorkAreaBounds::unbounded)
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

    /// Of the given monitor work areas, return the one the rect `(x, y, w, h)`
    /// overlaps most, or `None` if the slice is empty or the rect overlaps none
    /// of them. Lets the resize clamp target the monitor the window actually
    /// sits on instead of trusting `current_monitor()` near a DPI boundary.
    pub(crate) fn best_overlap(
        areas: &[WorkAreaBounds],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Option<WorkAreaBounds> {
        areas
            .iter()
            .copied()
            .map(|b| (b, b.intersection_area(x, y, w, h)))
            .filter(|(_, area)| *area > 0)
            .max_by_key(|(_, area)| *area)
            .map(|(bounds, _)| bounds)
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

/// One-time startup wiring for the vertical resize lock (see [`resize_lock`]).
/// The lock starts released and is engaged by `apply()` once a fit mode is in
/// effect, so this is inert until the user picks Up or Down.
pub fn install_resize_lock(window: &WebviewWindow) {
    resize_lock::install(window);
}

/// Logical-pixel minimum width enforced while the widget is in compact view,
/// below the non-compact `minWidth` from `tauri.conf.json`. Low enough that the
/// auto-fitted header width (what actually sizes the window in compact) is
/// reachable, high enough that a manual drag can't squash the widget to a
/// sliver. The non-compact floor stays the declared `minWidth` (single source),
/// restored when compact turns off.
const COMPACT_MIN_WIDTH: f64 = 140.0;

/// The window's declared `minWidth` from `tauri.conf.json` (the non-compact
/// floor), or 0.0 when the window has no entry / no declared minimum.
fn declared_min_width(window: &WebviewWindow) -> f64 {
    let app = window.app_handle();
    app.config()
        .app
        .windows
        .iter()
        .find(|w| w.label == window.label())
        .and_then(|w| w.min_width)
        .unwrap_or(0.0)
}

/// Relax (compact) or restore (non-compact) the window's minimum width so
/// compact view can shrink below the non-compact `minWidth` floor.
///
/// Each platform enforces the minimum through a different owner, so each is set
/// where it actually takes effect:
/// - Windows/Linux: tao's stored min inner size via `set_min_size` — nothing
///   else writes it, so this sticks until restored.
/// - macOS: the min width is one of the four bounds [`resize_lock`] rewrites
///   atomically on every height apply, so a bare `set_min_size` would be undone
///   on the next measure. Route through `resize_lock::refresh`, which rebuilds
///   the constraints reading the *current* `compact_mode` (the value the caller
///   persisted before invoking this).
pub fn set_compact_min_width(window: &WebviewWindow, compact: bool) {
    #[cfg(target_os = "macos")]
    {
        let _ = compact; // resize_lock reads compact_mode from config itself
        resize_lock::refresh(window);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let min_w = if compact { COMPACT_MIN_WIDTH } else { declared_min_width(window) };
        if let Err(e) = window.set_min_size(Some(tauri::LogicalSize::new(min_w, 0.0))) {
            tracing::warn!(?e, compact, "set_compact_min_width failed");
        }
    }
}

/// Clamp an outer rect `(x, y, w, h)` into the work area of the monitor it most
/// overlaps (primary as fallback), returning the adjusted top-left. Shared by
/// the compact-width resize so a width change (which moves the anchored edge)
/// can't push the widget off-screen — the same clamp `apply` uses for height.
pub(crate) fn clamp_to_work_area(
    window: &WebviewWindow,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> (i32, i32) {
    clamp_bounds(window, x, y, w, h).clamp(x, y, w, h)
}

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

/// Keep a manual drag from fighting auto-resize: while a fit mode owns the
/// height, an edge drag that changed it would be undone by the next measure
/// anyway. Only the *vertical* axis is locked — width stays the user's on both
/// platforms.
///
/// The two platforms need different mechanisms. Windows neutralizes the
/// top/bottom/corner hit-tests so the OS never enters a vertical resize loop.
/// macOS pins the window's min *and* max content height to the height `apply`
/// is about to set; AppKit enforces those bounds for the whole live-resize
/// path, so the edge simply doesn't move. An undecorated NSWindow still
/// carries `NSWindowStyleMask::Resizable` and is edge-resizable like any
/// other, which is why the Windows-only hit-test subclass left macOS with a
/// draggable height that auto-resize then snapped back.
///
/// `engage` is called before each resize and `release` when the mode goes back
/// to `None`; both are idempotent.
///
/// The macOS pin binds *programmatic* resizes too, so a `commands::set_window_size`
/// on the main window — `SetupPanel` widening the widget to fit the onboarding
/// copy — has its height clamped to the pin while a fit mode is on. That is
/// harmless rather than something to coordinate around: mounting the panel
/// flips `showSetup`, whose `$effect` in `App.svelte` schedules a measure, and
/// the panel's height can neither dedup (it's far from the last request) nor
/// read as non-overflowing, so auto-resize re-applies and re-pins at the
/// panel's own height a frame later — uncapped, where `SetupPanel` would have
/// capped itself at 2/3 of the screen. The width, which is the part only
/// `set_window_size` can do, is never clamped.
#[cfg(windows)]
mod resize_lock {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::WebviewWindow;

    /// Install the WM_NCHITTEST + WM_NCLBUTTONDOWN subclass on the window.
    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            tracing::warn!("resize lock: hwnd unavailable");
            return;
        };
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        let ok = unsafe {
            SetWindowSubclass(hwnd.0 as Hwnd, Some(subclass_proc), SUBCLASS_ID, 0)
        };
        if ok == 0 {
            INSTALLED.store(false, Ordering::SeqCst);
            tracing::warn!("SetWindowSubclass failed for resize lock");
        } else {
            tracing::debug!("resize-lock subclass installed");
        }
    }

    /// The height is irrelevant here — the subclass blocks the drag outright
    /// rather than bounding it.
    pub fn engage(_window: &WebviewWindow, _height_phys: i32) {
        LOCK_ACTIVE.store(true, Ordering::Relaxed);
    }

    pub fn release(_window: &WebviewWindow) {
        LOCK_ACTIVE.store(false, Ordering::Relaxed);
    }

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

#[cfg(target_os = "macos")]
mod resize_lock {
    use std::sync::atomic::{AtomicI32, Ordering};
    use tauri::utils::config::WindowConfig;
    use tauri::{LogicalUnit, Manager, PhysicalUnit, PixelUnit, WebviewWindow, WindowSizeConstraints};

    /// The height currently pinned by the lock, or `i32::MIN` when released.
    /// Remembered so [`refresh`] can rebuild the constraints — after a
    /// compact-mode min-width change — without a fresh height to pin. Stays a
    /// no-op sentinel until `engage` runs, so a refresh before any height apply
    /// simply leaves the height unpinned.
    static PINNED_HEIGHT: AtomicI32 = AtomicI32::new(i32::MIN);

    /// Nothing to install — the pin is applied per resize by `engage`.
    pub fn install(_window: &WebviewWindow) {}

    pub fn engage(window: &WebviewWindow, height_phys: i32) {
        PINNED_HEIGHT.store(height_phys, Ordering::Relaxed);
        set_constraints(window, Some(height_phys));
    }

    pub fn release(window: &WebviewWindow) {
        PINNED_HEIGHT.store(i32::MIN, Ordering::Relaxed);
        set_constraints(window, None);
    }

    /// Re-apply the constraints with the currently-pinned height (if any),
    /// picking up a changed `compact_mode` for the min-width bound. Called when
    /// compact view toggles so the min-width relaxes/restores immediately
    /// instead of waiting for the next height apply to rebuild the four bounds.
    pub fn refresh(window: &WebviewWindow) {
        let pinned = PINNED_HEIGHT.load(Ordering::Relaxed);
        let pin = (pinned != i32::MIN).then_some(pinned);
        set_constraints(window, pin);
    }

    fn set_constraints(window: &WebviewWindow, pinned_height_phys: Option<i32>) {
        let app = window.app_handle();
        let declared = app.config().app.windows.iter().find(|w| w.label == window.label());
        // Compact view is allowed below the declared floor; every other mode
        // keeps it. Reading `compact_mode` here (rather than passing it in) lets
        // both the per-resize path and `refresh` stay in sync with one source.
        let compact = window
            .try_state::<crate::config::ConfigState>()
            .map(|s| s.snapshot().compact_mode)
            .unwrap_or(false);
        let min_width = if compact { super::COMPACT_MIN_WIDTH } else { super::declared_min_width(window) };
        let constraints = constraints_with_pin(declared, min_width, pinned_height_phys);
        if let Err(e) = window.set_size_constraints(constraints) {
            tracing::warn!(?e, ?pinned_height_phys, compact, "resize lock: set_size_constraints failed");
        }
    }

    /// The window's `tauri.conf.json` bounds with the height axis pinned to
    /// `pinned_height_phys`, or left as declared when there's nothing to pin.
    ///
    /// Rebuilt from the declared config rather than from a remembered copy of
    /// what we last set, because the underlying call writes all four bounds at
    /// once: reconstructing them keeps `tauri.conf.json` (today `minWidth`) the
    /// single source of truth instead of a second copy here that would silently
    /// drop a bound added there later.
    ///
    /// The pin is expressed in physical pixels — the unit the height is
    /// measured and applied in — so it can't disagree with the resize by a
    /// scale-conversion rounding step and leave AppKit nudging the window by a
    /// pixel to satisfy its own constraint.
    ///
    /// Every field is filled in: a `None` there is not "leave this axis alone"
    /// but a sentinel tao expands to `f64::MIN` / `f64::MAX`, and those reach
    /// `NSWindow.setMinSize:` / `setMaxSize:` verbatim — a negative minimum
    /// size, and a maximum that overflows the frame arithmetic AppKit runs it
    /// through. `UNBOUNDED_MAX` is tao's own no-maximum value, so a released
    /// lock leaves the window in exactly the state tao would have.
    fn constraints_with_pin(
        declared: Option<&WindowConfig>,
        min_width: f64,
        pinned_height_phys: Option<i32>,
    ) -> WindowSizeConstraints {
        let logical = |v: f64| PixelUnit::Logical(LogicalUnit::new(v));
        let pin = || pinned_height_phys.map(|h| PixelUnit::Physical(PhysicalUnit::new(h)));
        let declared_or = |declared: Option<f64>, fallback: f64| {
            Some(logical(declared.unwrap_or(fallback)))
        };
        WindowSizeConstraints {
            // Caller-resolved (compact view relaxes it below the declared floor);
            // the other three bounds still come from `tauri.conf.json`.
            min_width: Some(logical(min_width)),
            min_height: pin()
                .or_else(|| declared_or(declared.and_then(|d| d.min_height), 0.0)),
            max_width: declared_or(declared.and_then(|d| d.max_width), UNBOUNDED_MAX),
            max_height: pin()
                .or_else(|| declared_or(declared.and_then(|d| d.max_height), UNBOUNDED_MAX)),
        }
    }

    /// Logical size tao itself passes to `NSWindow.setMaxSize:` for "no
    /// maximum" — large enough to never bind, small enough to stay well inside
    /// what AppKit's frame math handles.
    const UNBOUNDED_MAX: f64 = f32::MAX as f64;

    #[cfg(test)]
    mod tests {
        use super::*;

        fn declared() -> WindowConfig {
            // Mirrors the main window's tauri.conf.json entry: a min width, no
            // height bounds of its own.
            WindowConfig { min_width: Some(300.0), ..Default::default() }
        }

        fn logical(v: f64) -> Option<PixelUnit> {
            Some(PixelUnit::Logical(LogicalUnit::new(v)))
        }

        fn pinned(v: i32) -> Option<PixelUnit> {
            Some(PixelUnit::Physical(PhysicalUnit::new(v)))
        }

        #[test]
        fn engaging_pins_both_height_bounds_to_the_applied_height() {
            let c = constraints_with_pin(Some(&declared()), 300.0, Some(341));
            assert_eq!(c.min_height, pinned(341), "min height pinned");
            assert_eq!(c.max_height, pinned(341), "max height pinned — min == max is the lock");
        }

        #[test]
        fn pinning_the_height_keeps_the_declared_width_bounds() {
            // The whole point of rebuilding from the config: writing all four
            // bounds must not quietly drop the configured minimum width, which
            // would let the user squash the widget to nothing horizontally.
            let c = constraints_with_pin(Some(&declared()), 300.0, Some(341));
            assert_eq!(c.min_width, logical(300.0));
            assert_eq!(c.max_width, logical(UNBOUNDED_MAX), "no max width declared, none imposed");
        }

        #[test]
        fn releasing_frees_the_height_and_keeps_the_declared_width_bounds() {
            let c = constraints_with_pin(Some(&declared()), 300.0, None);
            assert_eq!(c.min_height, logical(0.0), "height free again");
            assert_eq!(c.max_height, logical(UNBOUNDED_MAX), "height free again");
            assert_eq!(c.min_width, logical(300.0));
        }

        #[test]
        fn every_bound_is_spelled_out_rather_than_left_unset() {
            // A None field is not "leave it alone" — tao expands it to
            // f64::MIN/f64::MAX and hands that straight to NSWindow.
            let c = constraints_with_pin(None, 0.0, None);
            assert!(
                [c.min_width, c.min_height, c.max_width, c.max_height]
                    .iter()
                    .all(Option::is_some),
                "no bound may be left for tao's MIN/MAX sentinels to fill in"
            );
        }

        #[test]
        fn released_height_falls_back_to_declared_height_bounds() {
            // A minHeight/maxHeight added to tauri.conf.json later must survive
            // a release rather than being cleared by it.
            let declared =
                WindowConfig { min_height: Some(120.0), max_height: Some(900.0), ..declared() };
            let c = constraints_with_pin(Some(&declared), 300.0, None);
            assert_eq!(c.min_height, logical(120.0));
            assert_eq!(c.max_height, logical(900.0));
        }

        #[test]
        fn pin_wins_over_declared_height_bounds() {
            let declared =
                WindowConfig { min_height: Some(120.0), max_height: Some(900.0), ..declared() };
            let c = constraints_with_pin(Some(&declared), 300.0, Some(341));
            assert_eq!(c.min_height, pinned(341), "the applied height overrides the declared floor");
            assert_eq!(c.max_height, pinned(341), "…and the declared ceiling");
        }

        #[test]
        fn undeclared_window_still_gets_the_pin() {
            // A window missing from the config (renamed label) must still lock,
            // just without any width bounds to preserve.
            let c = constraints_with_pin(None, 0.0, Some(341));
            assert_eq!(c.min_height, pinned(341));
            assert_eq!(c.min_width, logical(0.0));
        }

        #[test]
        fn compact_min_width_relaxes_below_the_declared_floor() {
            // Compact view resolves a min width below the declared 300 floor so
            // the widget can shrink to fit just the header; the height pin and
            // the other bounds are untouched by that.
            let c = constraints_with_pin(Some(&declared()), super::super::COMPACT_MIN_WIDTH, Some(341));
            assert_eq!(c.min_width, logical(super::super::COMPACT_MIN_WIDTH), "compact floor passes through, below declared 300");
            assert!(super::super::COMPACT_MIN_WIDTH < 300.0, "compact floor is genuinely below the declared minimum");
            assert_eq!(c.min_height, pinned(341), "height pin unaffected by the width relax");
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod resize_lock {
    use tauri::WebviewWindow;

    pub fn install(_window: &WebviewWindow) {}
    pub fn engage(_window: &WebviewWindow, _height_phys: i32) {}
    pub fn release(_window: &WebviewWindow) {}
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

/// Reads the window's TRUE client-area height and per-monitor DPI straight from
/// Win32, so `auto_resize::apply` can log requested-vs-actual height and
/// OS-DPI-vs-Tao-scale — the mixed-DPI drift diagnostics.
#[cfg(windows)]
mod geom {
    use tauri::WebviewWindow;

    // left/right are written by GetClientRect but we only read the height —
    // allow the never-read fields since the layout must match RECT exactly.
    #[repr(C)]
    #[allow(dead_code)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn GetDpiForWindow(hwnd: isize) -> u32;
    }

    /// `(client_height_phys, dpi)` read from Win32; `None` if the hwnd is
    /// unavailable or GetClientRect fails. The app process is per-monitor-DPI
    /// aware, so these are true physical pixels (no DPI virtualization).
    pub fn actual_client_and_dpi(window: &WebviewWindow) -> Option<(i32, u32)> {
        let hwnd = window.hwnd().ok()?.0 as isize;
        let mut r = Rect { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
            return None;
        }
        Some((r.bottom - r.top, unsafe { GetDpiForWindow(hwnd) }))
    }
}

#[cfg(not(windows))]
mod geom {
    use tauri::WebviewWindow;

    pub fn actual_client_and_dpi(_window: &WebviewWindow) -> Option<(i32, u32)> {
        None
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

    #[test]
    fn best_overlap_picks_monitor_holding_the_window() {
        // Primary 2560x1440 at the origin with a second monitor to its right
        // (work area x=2560..5000). A window at x=4137 sits entirely on the
        // right monitor: best_overlap must return it so the clamp is a no-op —
        // clamping against the *primary* instead is what marched the widget to
        // a corner near the DPI boundary.
        let primary = WorkAreaBounds { left: 0, top: 0, right: 2560, bottom: 1440 };
        let right = WorkAreaBounds { left: 2560, top: 0, right: 5000, bottom: 2000 };
        let areas = [primary, right];
        let picked = WorkAreaBounds::best_overlap(&areas, 4137, 1400, 630, 340).unwrap();
        assert_eq!(
            picked.clamp(4137, 1400, 630, 340),
            (4137, 1400),
            "clamp against the correct (right) monitor leaves the window put"
        );
        assert_ne!(
            primary.clamp(4137, 1400, 630, 340),
            (4137, 1400),
            "clamp against the wrong (primary) monitor would move it — the bug"
        );
    }

    #[test]
    fn best_overlap_prefers_the_monitor_with_more_of_the_window() {
        // Window straddling the seam but mostly on the right monitor.
        let left = WorkAreaBounds { left: 0, top: 0, right: 2560, bottom: 1440 };
        let right = WorkAreaBounds { left: 2560, top: 0, right: 5000, bottom: 1440 };
        let areas = [left, right];
        // x=2460..3100: 100px on the left monitor, 540px on the right.
        let picked = WorkAreaBounds::best_overlap(&areas, 2460, 200, 640, 400).unwrap();
        assert_eq!(picked.left, 2560, "chose the right monitor (larger overlap)");
    }

    #[test]
    fn best_overlap_none_when_stranded_off_all_monitors() {
        let primary = WorkAreaBounds { left: 0, top: 0, right: 2560, bottom: 1440 };
        // Window fully to the right of every monitor (disconnected display) —
        // no overlap, so apply() falls back to the primary for the rescue clamp.
        assert!(WorkAreaBounds::best_overlap(&[primary], 6000, 500, 630, 340).is_none());
    }

    #[test]
    fn best_overlap_none_for_empty_monitor_list() {
        assert!(WorkAreaBounds::best_overlap(&[], 0, 0, 100, 100).is_none());
    }
}
