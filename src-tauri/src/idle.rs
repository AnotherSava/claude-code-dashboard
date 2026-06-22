//! System-wide user-input idle time, used by the notification reconciler's
//! AFK gate. Reports milliseconds since the last keyboard/mouse event across
//! the whole desktop session — not just this app's windows — so "the user has
//! stepped away" can be distinguished from "the user is here but hasn't queued
//! the next task". `None` means the query failed or the platform is
//! unsupported; the reconciler treats that as "presence unknown" and never
//! fires the AFK path on it (the reaction-window backstop still applies).

/// Milliseconds since the last system-wide keyboard or mouse input.
#[cfg(windows)]
pub fn idle_ms() -> Option<u64> {
    #[repr(C)]
    struct LastInputInfo {
        cb_size: u32,
        dw_time: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount() -> u32;
    }

    let mut lii = LastInputInfo {
        cb_size: std::mem::size_of::<LastInputInfo>() as u32,
        dw_time: 0,
    };
    if unsafe { GetLastInputInfo(&mut lii) } == 0 {
        return None;
    }
    // Both values are 32-bit millisecond tick counts since boot that wrap
    // every ~49.7 days; wrapping_sub yields the correct delta across one wrap.
    Some(unsafe { GetTickCount() }.wrapping_sub(lii.dw_time) as u64)
}

/// Milliseconds since the last system-wide keyboard or mouse input.
#[cfg(target_os = "macos")]
pub fn idle_ms() -> Option<u64> {
    // CGEventSourceStateID::kCGEventSourceStateCombinedSessionState = 0,
    // CGEventType::kCGAnyInputEventType = 0xFFFFFFFF.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    }

    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(0, 0xFFFF_FFFF) };
    if secs.is_finite() && secs >= 0.0 {
        Some((secs * 1000.0) as u64)
    } else {
        None
    }
}

/// Milliseconds since the last system-wide keyboard or mouse input.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn idle_ms() -> Option<u64> {
    None
}
