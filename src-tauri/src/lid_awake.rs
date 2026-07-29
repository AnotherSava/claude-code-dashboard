//! Keeps macOS awake with the lid closed while a local agent is working — the
//! "carry the laptop to another room mid-task" case.
//!
//! # Why this is not a power assertion
//!
//! Lid close is not idle sleep. It is a distinct kernel path ("Clamshell
//! Sleep"), and Apple's own `IOPMLib.h` says of the strongest public idle
//! assertion: "the system may still sleep for lid close, Apple menu, low
//! battery, or other sleep reasons". Every assertion type created with the
//! private `AppliesOnLidClose` property is refused with `kIOReturnNotPrivileged`
//! unless the caller holds `com.apple.private.iokit.assertonlidclose`, which
//! third parties cannot obtain — `/usr/bin/caffeinate` does not hold it either,
//! which is why no `caffeinate` flag helps.
//!
//! The only remaining lever is `pmset -a disablesleep`, which sets the kernel's
//! `userDisabledAllSleep`. That is checked in `checkSystemSleepAllowed` *before*
//! and independent of the sleep reason, so it stops clamshell sleep too. It is
//! root-only (`IOPMSetSystemPowerSetting` gates on a hardcoded `getuid() != 0`,
//! not an entitlement — no code-signing identity would change this), so we reach
//! it through a one-time, argument-pinned sudoers grant. That is the same
//! approach Apple's TN2065 recommends and the same one Amphetamine's "Power
//! Protect" uses.
//!
//! # Why every hold is bounded
//!
//! `disablesleep` is a *system-wide sleep kill switch*, not a lid-close switch:
//! it also suppresses thermal-emergency and low-battery sleep. A laptop left
//! vetoed in a bag would neither sleep nor protect itself. So the hold is capped
//! by [`Config::lid_awake_minutes`], floored by
//! [`Config::lid_awake_battery_floor_pct`], and covered by four independent
//! recovery paths (see "Safety" below).
//!
//! # Lease semantics — anchored to the lid, not to the work
//!
//! The veto has to be armed *before* the lid closes: with sleep enabled the Mac
//! is out the instant the lid shuts, so there is no window in which to react.
//! But once armed the machine stays up, so the *countdown* can be timed against
//! the lid instead:
//!
//! - agent becomes busy → veto arms, countdown not running
//! - lid closes → countdown starts from zero → a full window of carrying it shut
//! - lid reopens → countdown resets; the veto stays armed while work continues
//! - work ends (plus a grace), or the lease expires with the lid still shut, or
//!   the battery floor is breached → release
//!
//! So "15 minutes" means fifteen minutes of carrying it closed, not fifteen
//! minutes of uptime. The accepted cost is that `SleepDisabled` also stays set
//! through lid-*open* agent work, since pre-arming is unavoidable;
//! [`LidAwakeMode::OnBattery`] exists to keep it off during docked desk work.
//!
//! # Safety
//!
//! Each layer covers a failure the others cannot:
//!
//! 1. **Lease expiry** — the bounded closed-lid window, the primary control.
//! 2. **Battery floor** — never arm below the floor; release if it is crossed.
//! 3. **Deadman** — a detached `/bin/sh` watcher clears the flag when this
//!    process dies. Reparented to launchd, it survives `SIGKILL`, Force Quit,
//!    and the tray's `std::process::exit(0)`, which bypasses `Drop` entirely.
//! 4. **Boot-reset LaunchDaemon** — `disablesleep` *persists across reboot*
//!    (powerd re-applies it from disk at every boot), so "reboot to recover" is
//!    false and a boot-time reset job is required rather than optional.
//! 5. **Clear-on-start** — startup always releases before anything else.
//!
//! Every arm is verified against the kernel's own `SleepDisabled`, so a silently
//! failed `pmset` is reported rather than assumed to have worked.
//!
//! # Policy vs. request
//!
//! [`LidAwakeMode`] is the *standing policy* for arming automatically. The
//! tray's "Start now" is a separate one-shot request that overrides it — `Off`
//! included — for the "about to pick the laptop up" case. It is not a policy
//! value: a mode whose only behavior is "don't arm automatically" would be
//! indistinguishable from `Off`.
//!
//! The tray still presents both in one radio group, with the check-mark on
//! whatever is actually in effect — a running hold, else the policy. That is
//! only honest because [`manual_remaining_ms`] gates the countdown on the veto
//! being *armed* rather than merely requested: a check-mark that outlived the
//! hold, or one shown for an arm that silently failed, would promise a Mac that
//! stays awake when it will sleep the moment the lid shuts.
//!
//! macOS-only; every entry point is a no-op elsewhere.

use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::{ConfigState, LidAwakeMode};
use crate::state::{AgentSession, AppState, Status};

/// Poll cadence. Drives lid-state detection, lease expiry, and the battery
/// floor. The lease is minutes-scale, so this only needs to be fine enough that
/// the countdown starts promptly after the lid shuts.
const POLL: Duration = Duration::from_secs(5);

/// How long to wait before retrying after a failed arm. Without it, a missing
/// or declined sudoers grant would fork a `sudo` and write a log line on every
/// tick for as long as any agent stayed busy.
const ARM_RETRY_BACKOFF_MS: i64 = 60_000;

/// `/etc/sudoers.d` entry name. **Must not contain a dot** — sudo silently skips
/// files in an include directory whose names contain `.` or end in `~`, so
/// naming this after the bundle id would leave the grant permanently inert.
const SUDOERS_FILE: &str = "claude-code-dashboard-lidawake";

/// Boot-time reset job. Its `Program` is `/usr/bin/pmset` — an Apple platform
/// binary on the sealed system volume — so this app's own (ad-hoc) signature is
/// irrelevant to launchd, unlike `SMAppService`.
const DAEMON_LABEL: &str = "com.anothersava.claude-code-dashboard.sleepreset";

const PMSET_BIN: &str = "/usr/bin/pmset";

/// The exact argv pinned by the sudoers rule. sudo matches arguments literally
/// when they are specified, so this cannot be widened into a general root shell
/// — but it also means the rule and every call site must agree character for
/// character. This function is the single source all three build from (the
/// sudoers generator, the runtime toggle, and the deadman script).
fn pmset_args(on: bool) -> [&'static str; 3] {
    ["-a", "disablesleep", if on { "1" } else { "0" }]
}

fn pmset_cmdline(on: bool) -> String {
    format!("{PMSET_BIN} {}", pmset_args(on).join(" "))
}

fn sudoers_path() -> String {
    format!("/etc/sudoers.d/{SUDOERS_FILE}")
}

fn daemon_path() -> String {
    format!("/Library/LaunchDaemons/{DAEMON_LABEL}.plist")
}

// ---------------------------------------------------------------------------
// Pure decision layer (cross-platform so the tests run everywhere)
// ---------------------------------------------------------------------------

/// Everything the arm/release decision reads: the live environment plus the
/// configured bounds. Pure input — no clock reads, no IO — so the policy can be
/// exercised exhaustively in tests.
#[derive(Clone, Copy, Debug)]
struct Sample {
    mode: LidAwakeMode,
    /// Any *local* session in `Working` or `Waiting`.
    any_busy: bool,
    /// Running on battery (i.e. no power adapter attached).
    on_battery: bool,
    /// Charge percentage, or `None` when there is no battery (a desktop) or the
    /// read failed.
    battery_pct: Option<u8>,
    floor_pct: u8,
    lease_ms: u64,
    grace_ms: u64,
    now: i64,
}

impl Sample {
    /// Whether the battery is above the floor. An unknown level does not block
    /// (a desktop Mac reports none), and a zero floor disables the check.
    fn battery_ok(&self) -> bool {
        self.floor_pct == 0 || self.battery_pct.map_or(true, |p| p >= self.floor_pct)
    }

    /// Whether the veto *should* be held right now.
    ///
    /// `manual_until` is a live "Start now" request. It overrides the standing
    /// policy — `Off` included — because the user asked for this hold directly.
    /// `evaluate` re-stamps it from the lid-close edge, so the value seen here
    /// is already lid-anchored and needs no special case for a shut lid.
    fn wants_arm(&self, manual_until: Option<i64>) -> bool {
        if self.lease_ms == 0 || !self.battery_ok() {
            return false;
        }
        if manual_until.is_some_and(|t| self.now < t) {
            return true;
        }
        match self.mode {
            LidAwakeMode::Off => false,
            LidAwakeMode::OnBattery => self.any_busy && self.on_battery,
            LidAwakeMode::Always => self.any_busy,
        }
    }

    /// Whether an already-armed veto must drop.
    ///
    /// `lid_closed_since` is the instant the lid last shut while armed, and is
    /// the *only* thing the lease is measured against — a session that works for
    /// hours with the lid open never expires.
    fn wants_release(&self, lid_closed_since: Option<i64>, last_busy_at: i64, manual_until: Option<i64>) -> bool {
        // Battery floor overrides everything, including an explicit manual hold:
        // `disablesleep` suppresses the low-battery emergency sleep, so a closed
        // laptop would otherwise run itself flat rather than sleeping.
        if !self.battery_ok() {
            return true;
        }
        if let Some(closed_at) = lid_closed_since {
            if self.now.saturating_sub(closed_at) >= self.lease_ms as i64 {
                return true;
            }
        }
        if self.wants_arm(manual_until) {
            return false;
        }
        // Conditions no longer hold. The grace exists to ride out the Working →
        // Done → Working flap between turns, so it only applies to the policies
        // that actually watch sessions: under `Off` the hold was a one-shot
        // "Start now", and once that lapses there is nothing to flap back to —
        // waiting on `last_busy_at` there would keep holding for as long as any
        // agent happened to be working.
        match self.mode {
            LidAwakeMode::Off => true,
            LidAwakeMode::OnBattery | LidAwakeMode::Always => {
                self.now.saturating_sub(last_busy_at) >= self.grace_ms as i64
            }
        }
    }
}

/// Why the veto dropped, for the decision log.
fn release_reason(s: &Sample, lid_closed_since: Option<i64>) -> &'static str {
    if !s.battery_ok() {
        "battery fell below the floor; released so low-battery sleep can protect the machine"
    } else if lid_closed_since.is_some() {
        "closed-lid lease expired; allowing the Mac to sleep"
    } else if s.mode == LidAwakeMode::Off {
        // A deliberate policy change releases through `release_now` with its own
        // reason, so reaching here under `Off` means a "Start now" hold lapsed
        // without the lid ever being shut.
        "the requested window ended without the lid being closed"
    } else {
        "no local agent busy past the release grace"
    }
}

/// True when a status counts as work that sleeping would suspend.
///
/// `Waiting` counts: it means the main turn settled but a background shell task
/// or subagent is still running. `Blocked` does not — the agent is parked on the
/// user, so nothing progresses while the Mac sleeps.
fn is_busy(status: Status) -> bool {
    matches!(status, Status::Working | Status::Waiting)
}

// ---------------------------------------------------------------------------
// Tracked state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    armed: bool,
    /// When the lid last shut while armed. `None` whenever the lid is open.
    lid_closed_since: Option<i64>,
    /// Last moment a local session was busy — the anchor for the release grace.
    last_busy_at: i64,
    /// Fallback expiry for a manual hold whose lid never closed.
    manual_until: Option<i64>,
    /// Earliest retry after a failed arm — see [`ARM_RETRY_BACKOFF_MS`].
    arm_retry_after: i64,
    /// Detached watcher that clears the flag if this process dies.
    deadman: Option<std::process::Child>,
}

#[derive(Default)]
pub struct LidAwakeState {
    inner: std::sync::Mutex<Inner>,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Record live busy state and re-evaluate. Called from
/// `commands::emit_sessions_updated`, the chokepoint every status transition
/// already flows through, so arming reacts immediately rather than at the next
/// tick. Local sessions only — a remote row is another machine's work.
pub fn sync(app: &AppHandle, sessions: &[AgentSession]) {
    let any_busy = sessions.iter().any(|s| is_busy(s.status));
    if let Some(state) = app.try_state::<LidAwakeState>() {
        if any_busy {
            state.inner.lock().unwrap().last_busy_at = now_ms();
        }
    }
    evaluate(app, any_busy);
}

/// Periodic tick: detects the lid closing, expires the lease, and enforces the
/// battery floor — none of which produce a session event to hang off.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("lid-awake watcher started");

        loop {
            ticker.tick().await;
            let any_busy = app
                .try_state::<AppState>()
                .is_some_and(|st| st.snapshot().iter().any(|s| is_busy(s.status)));
            evaluate(&app, any_busy);
        }
    });
}

/// Arm one window on explicit request (the tray's "Start now"). Overrides the
/// standing policy — including `Off` — and re-invoking it refreshes the window.
pub fn arm_manual(app: &AppHandle) {
    let Some(state) = app.try_state::<LidAwakeState>() else { return };
    let lease_ms = lease_ms_of(app);
    if lease_ms == 0 {
        return;
    }
    state.inner.lock().unwrap().manual_until = Some(now_ms() + lease_ms as i64);
    let any_busy = app
        .try_state::<AppState>()
        .is_some_and(|st| st.snapshot().iter().any(|s| is_busy(s.status)));
    evaluate(app, any_busy);
}

/// Drop the veto now, whatever the mode says. Used when the policy is switched
/// off and at startup.
pub fn release_now(app: &AppHandle, reason: &str) {
    let Some(state) = app.try_state::<LidAwakeState>() else { return };
    {
        let mut inner = state.inner.lock().unwrap();
        inner.manual_until = None;
        if inner.armed {
            do_release(&mut inner, reason);
        }
    }
    refresh_tray(app);
}

/// Cancel any outstanding "Start now" request without touching the standing
/// policy. Picking a policy item is an explicit choice, so the one-shot overlay
/// steps aside and the check-mark follows the selection.
pub fn cancel_manual(app: &AppHandle) {
    let Some(state) = app.try_state::<LidAwakeState>() else { return };
    state.inner.lock().unwrap().manual_until = None;
}

/// Milliseconds left on a live "Start now" hold, or `None` when none is in
/// force. Read by the tray to render the countdown and move the check-mark.
///
/// Gated on `armed`, not merely on an outstanding request: an arm that failed
/// (no sudoers grant, or the user declined setup) leaves the request standing
/// while nothing is actually held, and a ticking countdown there would promise a
/// Mac that stays awake when it will sleep the moment the lid shuts.
pub fn manual_remaining_ms(app: &AppHandle) -> Option<i64> {
    let state = app.try_state::<LidAwakeState>()?;
    let inner = state.inner.lock().unwrap();
    if !inner.armed {
        return None;
    }
    let left = inner.manual_until?.saturating_sub(now_ms());
    (left > 0).then_some(left)
}

/// Push the current policy + manual countdown into the tray menu.
fn refresh_tray(app: &AppHandle) {
    let Some(cfg) = app.try_state::<ConfigState>() else { return };
    let mode = cfg.snapshot().lid_awake_mode;
    crate::tray::sync_lid_awake_state(app, mode, manual_remaining_ms(app));
}

fn lease_ms_of(app: &AppHandle) -> u64 {
    app.try_state::<ConfigState>()
        .map(|c| c.snapshot().lid_awake_minutes.saturating_mul(60_000))
        .unwrap_or(0)
}

fn evaluate(app: &AppHandle, any_busy: bool) {
    decide(app, any_busy);
    // The countdown ticks down and the hold can lapse without any menu
    // interaction, so the tray is re-synced from here rather than only from the
    // click handlers. `sync_lid_awake_state` drops no-op updates, so this costs
    // nothing on the quiet ticks.
    refresh_tray(app);
}

fn decide(app: &AppHandle, any_busy: bool) {
    let (Some(cfg_state), Some(state)) = (app.try_state::<ConfigState>(), app.try_state::<LidAwakeState>()) else {
        return;
    };
    let cfg = cfg_state.snapshot();

    let mut inner = state.inner.lock().unwrap();
    let now = now_ms();
    let lease_ms = cfg.lid_awake_minutes.saturating_mul(60_000);

    // Nothing to do when no policy is set, nothing is held, and no "Start now"
    // is outstanding. A live manual request overrides `Off`, so it must not
    // short-circuit here.
    if cfg.lid_awake_mode == LidAwakeMode::Off
        && !inner.armed
        && !inner.manual_until.is_some_and(|t| now < t)
    {
        inner.manual_until = None;
        return;
    }

    let lid_closed = platform::lid_closed().unwrap_or(false);
    let (on_battery, battery_pct) = platform::power();

    // Track the lid edge: the lease is measured from the close, and reopening
    // resets it so a second leg of the journey gets a fresh window.
    match (lid_closed, inner.lid_closed_since) {
        (true, None) => {
            inner.lid_closed_since = Some(now);
            // Re-anchor a live manual hold to the close, so it gets the same
            // full window the policy modes do. Its original expiry only ever
            // governed a "Start now" whose lid never actually shut.
            if inner.manual_until.is_some() {
                inner.manual_until = Some(now + lease_ms as i64);
            }
        }
        (false, Some(_)) => inner.lid_closed_since = None,
        _ => {}
    }

    let sample = Sample {
        mode: cfg.lid_awake_mode,
        any_busy,
        on_battery,
        battery_pct,
        floor_pct: cfg.lid_awake_battery_floor_pct,
        lease_ms,
        grace_ms: cfg.lid_awake_release_grace_ms,
        now,
    };

    if inner.armed {
        if sample.wants_release(inner.lid_closed_since, inner.last_busy_at, inner.manual_until) {
            let reason = release_reason(&sample, inner.lid_closed_since);
            let held_ms = inner.lid_closed_since.map(|t| now.saturating_sub(t));
            inner.manual_until = None;
            do_release(&mut inner, reason);
            tracing::info!(
                decision = "lid_awake_release",
                mode = ?sample.mode,
                lid_closed,
                battery_pct = ?battery_pct,
                lid_closed_ms = ?held_ms,
                reason,
                "decision"
            );
        }
        return;
    }

    if !sample.wants_arm(inner.manual_until) {
        return;
    }
    // A failed arm (no grant, or the user declined setup) would otherwise fork a
    // `sudo` every tick for the whole time an agent is busy.
    if now < inner.arm_retry_after {
        return;
    }

    match platform::set_sleep_disabled(true) {
        Ok(()) => {
            inner.armed = true;
            inner.arm_retry_after = 0;
            inner.deadman = platform::spawn_deadman(std::process::id());
            // The kernel's own view is the only honest confirmation that the
            // veto took — a `pmset` that exits 0 without the flag actually
            // landing would otherwise read as success.
            let verified = platform::sleep_disabled();
            if verified == Some(false) {
                tracing::warn!(
                    decision = "lid_awake_arm",
                    mode = ?sample.mode,
                    reason = "pmset reported success but the kernel still shows SleepDisabled clear",
                    "decision"
                );
            } else {
                tracing::info!(
                    decision = "lid_awake_arm",
                    mode = ?sample.mode,
                    any_busy,
                    on_battery,
                    battery_pct = ?battery_pct,
                    lid_closed,
                    lease_ms = sample.lease_ms,
                    verified = ?verified,
                    reason = "a local agent is working; holding off sleep so a lid close doesn't suspend it",
                    "decision"
                );
            }
        }
        Err(e) => {
            inner.arm_retry_after = now + ARM_RETRY_BACKOFF_MS;
            tracing::warn!(
                decision = "lid_awake_arm",
                mode = ?sample.mode,
                error = %e,
                retry_in_ms = ARM_RETRY_BACKOFF_MS,
                reason = "could not set disablesleep; the sudoers grant is probably missing (re-run setup from the tray)",
                "decision"
            );
        }
    }
}

/// Clear the flag and retire the deadman. Assumes the caller holds the lock.
fn do_release(inner: &mut Inner, reason: &str) {
    if let Err(e) = platform::set_sleep_disabled(false) {
        tracing::warn!(error = %e, reason, "lid-awake release failed; the boot-reset daemon and deadman remain as backstops");
    }
    inner.armed = false;
    inner.lid_closed_since = None;
    if let Some(mut child) = inner.deadman.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Best-effort clear at startup, before anything else can arm. Covers a flag
/// stranded by a crash the deadman somehow missed.
pub fn clear_on_start() {
    if platform::installed() {
        let _ = platform::set_sleep_disabled(false);
    }
}

/// Whether the one-time privileged setup has been performed.
pub fn installed() -> bool {
    platform::installed()
}

/// Run the one-time privileged setup: install the argument-pinned sudoers grant
/// and the boot-reset daemon behind a single administrator prompt.
pub async fn install() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(platform::install)
        .await
        .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// Platform layer
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::{daemon_path, pmset_args, pmset_cmdline, sudoers_path, DAEMON_LABEL};
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_void};
    use std::process::{Command, Stdio};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFMutableDictionaryRef = *mut c_void;
    type CFAllocatorRef = *const c_void;
    type CFTypeID = usize;
    type CFIndex = isize;
    type IoObject = u32;

    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// `kCFNumberSInt64Type`
    const CF_NUMBER_SINT64: CFIndex = 4;
    /// `kIOMainPortDefault`
    const IO_MAIN_PORT_DEFAULT: u32 = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: CFAllocatorRef, cstr: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
        fn CFBooleanGetTypeID() -> CFTypeID;
        fn CFBooleanGetValue(b: CFTypeRef) -> u8;
        fn CFNumberGetTypeID() -> CFTypeID;
        fn CFNumberGetValue(n: CFTypeRef, the_type: CFIndex, value: *mut c_void) -> u8;
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
        fn IOServiceGetMatchingService(main_port: u32, matching: CFMutableDictionaryRef) -> IoObject;
        fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CFStringRef, allocator: CFAllocatorRef, options: u32) -> CFTypeRef;
        fn IOObjectRelease(obj: IoObject) -> c_int;
    }

    enum Prop {
        Bool(bool),
        Int(i64),
    }

    /// Read one property off the first matching IORegistry service. Returns
    /// `None` when the service or key is absent, so callers can degrade rather
    /// than guess (a desktop Mac has no `AppleSmartBattery`).
    fn read_prop(service: &str, key: &str) -> Option<Prop> {
        let service_c = CString::new(service).ok()?;
        let key_c = CString::new(key).ok()?;
        unsafe {
            let matching = IOServiceMatching(service_c.as_ptr());
            if matching.is_null() {
                return None;
            }
            // IOServiceGetMatchingService consumes the matching dictionary's
            // reference, so it must not be released here.
            let entry = IOServiceGetMatchingService(IO_MAIN_PORT_DEFAULT, matching);
            if entry == 0 {
                return None;
            }
            let key_cf = CFStringCreateWithCString(std::ptr::null(), key_c.as_ptr(), CF_STRING_ENCODING_UTF8);
            let value = if key_cf.is_null() {
                std::ptr::null()
            } else {
                IORegistryEntryCreateCFProperty(entry, key_cf, std::ptr::null(), 0)
            };
            if !key_cf.is_null() {
                CFRelease(key_cf);
            }
            IOObjectRelease(entry);

            if value.is_null() {
                return None;
            }
            let type_id = CFGetTypeID(value);
            let out = if type_id == CFBooleanGetTypeID() {
                Some(Prop::Bool(CFBooleanGetValue(value) != 0))
            } else if type_id == CFNumberGetTypeID() {
                let mut n: i64 = 0;
                if CFNumberGetValue(value, CF_NUMBER_SINT64, &mut n as *mut i64 as *mut c_void) != 0 {
                    Some(Prop::Int(n))
                } else {
                    None
                }
            } else {
                None
            };
            CFRelease(value);
            out
        }
    }

    fn read_bool(service: &str, key: &str) -> Option<bool> {
        match read_prop(service, key)? {
            Prop::Bool(b) => Some(b),
            Prop::Int(n) => Some(n != 0),
        }
    }

    fn read_int(service: &str, key: &str) -> Option<i64> {
        match read_prop(service, key)? {
            Prop::Int(n) => Some(n),
            Prop::Bool(b) => Some(b as i64),
        }
    }

    /// True when the lid is shut.
    pub(super) fn lid_closed() -> Option<bool> {
        read_bool("IOPMrootDomain", "AppleClamshellState")
    }

    /// The kernel's own copy of the sleep kill switch — the property `pmset
    /// disablesleep` writes and `checkSystemSleepAllowed` reads. This is what an
    /// arm is verified against.
    ///
    /// Deliberately *not* `AppleClamshellCausesSleep`, which looks like the more
    /// direct oracle but isn't: it was observed reading `No` on this machine
    /// while `pmset -g log` went on recording `Entering Sleep state due to
    /// 'Clamshell Sleep'`, so it reflects powerd's derived policy state rather
    /// than a live guarantee, and trusting it would vouch for a veto that never
    /// took.
    pub(super) fn sleep_disabled() -> Option<bool> {
        read_bool("IOPMrootDomain", "SleepDisabled")
    }

    /// `(on_battery, charge_percent)`. A Mac with no battery reports
    /// `(false, None)` — treated as mains power with no floor to enforce.
    pub(super) fn power() -> (bool, Option<u8>) {
        let external = read_bool("AppleSmartBattery", "ExternalConnected");
        let pct = match (read_int("AppleSmartBattery", "CurrentCapacity"), read_int("AppleSmartBattery", "MaxCapacity")) {
            (Some(cur), Some(max)) if max > 0 => Some((cur.saturating_mul(100) / max).clamp(0, 100) as u8),
            _ => None,
        };
        (external == Some(false), pct)
    }

    pub(super) fn set_sleep_disabled(on: bool) -> Result<(), String> {
        let out = Command::new("/usr/bin/sudo")
            .arg("-n")
            .arg(super::PMSET_BIN)
            .args(pmset_args(on))
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Detached watcher that clears the flag once this process is gone. Survives
    /// `SIGKILL` and the tray's `std::process::exit(0)` because it is reparented
    /// to launchd rather than torn down with us. Output is discarded so its
    /// messages can't leak into the user's terminal after we exit.
    pub(super) fn spawn_deadman(pid: u32) -> Option<std::process::Child> {
        let script = format!(
            "while kill -0 {pid} 2>/dev/null; do sleep 2; done; /usr/bin/sudo -n {}",
            pmset_cmdline(false)
        );
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    pub(super) fn installed() -> bool {
        // The file is 0440 root:wheel so it can't be read, but /etc/sudoers.d is
        // world-traversable, so stat succeeds.
        std::path::Path::new(&sudoers_path()).exists()
    }

    /// Escape a string for an AppleScript literal. Only backslash and double
    /// quote need it — and the script must stay newline-free, since AppleScript
    /// string literals cannot span lines (hence the `printf '%s\n'` idiom below).
    fn applescript_quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                _ => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    /// Single-quote a string for the shell. Used for lines embedded in the
    /// privileged script; none of our content contains a `'`, but this keeps
    /// that from becoming a silent injection if the username ever does.
    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn run_privileged(script: &str) -> Result<(), String> {
        let out = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!("do shell script {} with administrator privileges", applescript_quote(script)))
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("User canceled") || err.contains("-128") {
                Err("cancelled".to_string())
            } else {
                Err(err.trim().to_string())
            }
        }
    }

    /// The one-time privileged script, as a single line.
    ///
    /// It must stay newline-free: `do shell script` takes an AppleScript string
    /// literal, and those cannot span lines — hence `printf '%s\n' 'a' 'b' …`
    /// rather than a heredoc. Pure so the quoting can be exercised in tests and
    /// dry-run against scratch paths before it is ever handed to root.
    pub(super) fn install_script(user: &str, sudoers: &str, daemon: &str) -> String {
        // Arguments are pinned literally, so this grant cannot be widened into a
        // general root shell. No Digest_Spec: /usr/bin/pmset lives on the sealed,
        // read-only system volume under SIP, and a hash would break on every OS
        // update — silently disabling the grant.
        let rule = format!("{user} ALL=(root) NOPASSWD: {}, {}", pmset_cmdline(false), pmset_cmdline(true));

        let plist_lines = [
            r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_string(),
            r#"<plist version="1.0">"#.to_string(),
            "<dict>".to_string(),
            "<key>Label</key>".to_string(),
            format!("<string>{DAEMON_LABEL}</string>"),
            "<key>ProgramArguments</key>".to_string(),
            "<array>".to_string(),
            format!("<string>{}</string>", super::PMSET_BIN),
            "<string>-a</string>".to_string(),
            "<string>disablesleep</string>".to_string(),
            "<string>0</string>".to_string(),
            "</array>".to_string(),
            "<key>RunAtLoad</key>".to_string(),
            "<true/>".to_string(),
            "</dict>".to_string(),
            "</plist>".to_string(),
        ];

        // One line, because an AppleScript string literal cannot contain raw
        // newlines; `printf '%s\n' ...` emits the multi-line files instead.
        format!(
            "set -e; T=$(mktemp); printf '%s\\n' {rule} > \"$T\"; /usr/sbin/visudo -cf \"$T\"; \
             chown root:wheel \"$T\"; chmod 0440 \"$T\"; mv \"$T\" {sudoers}; \
             printf '%s\\n' {plist} > {daemon}; chown root:wheel {daemon}; chmod 0644 {daemon}; \
             launchctl bootout system {daemon} 2>/dev/null || true; launchctl bootstrap system {daemon}",
            rule = sh_quote(&rule),
            sudoers = sh_quote(sudoers),
            daemon = sh_quote(daemon),
            plist = plist_lines.iter().map(|l| sh_quote(l)).collect::<Vec<_>>().join(" "),
        )
    }

    pub(super) fn install() -> Result<(), String> {
        let user = std::env::var("USER").map_err(|_| "cannot determine the current user".to_string())?;
        run_privileged(&install_script(&user, &sudoers_path(), &daemon_path()))?;
        // Prove the grant actually works rather than trusting the installer's
        // exit code: clearing the flag is always safe and needs no password if
        // the rule matched.
        set_sleep_disabled(false).map_err(|e| format!("sudoers grant installed but not effective: {e}"))
    }

}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) fn lid_closed() -> Option<bool> {
        None
    }
    pub(super) fn sleep_disabled() -> Option<bool> {
        None
    }
    pub(super) fn power() -> (bool, Option<u8>) {
        (false, None)
    }
    pub(super) fn set_sleep_disabled(_on: bool) -> Result<(), String> {
        Err("lid-awake is macOS-only".to_string())
    }
    pub(super) fn spawn_deadman(_pid: u32) -> Option<std::process::Child> {
        None
    }
    pub(super) fn installed() -> bool {
        false
    }
    pub(super) fn install() -> Result<(), String> {
        Err("lid-awake is macOS-only".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: u64 = 900_000; // 15 min
    const GRACE: u64 = 60_000; // 1 min
    const MIN: i64 = 60_000;

    fn sample(mode: LidAwakeMode, any_busy: bool, now: i64) -> Sample {
        Sample {
            mode,
            any_busy,
            on_battery: true,
            battery_pct: Some(80),
            floor_pct: 20,
            lease_ms: LEASE,
            grace_ms: GRACE,
            now,
        }
    }

    /// The privileged script is handed to `osascript` as an AppleScript string
    /// literal, which cannot span lines — a stray newline would break the
    /// install with an opaque syntax error at the one moment we've asked the
    /// user for their password. It must also pin the same argv as the sudoers
    /// grant, or the rule silently stops matching and every arm fails.
    #[cfg(target_os = "macos")]
    #[test]
    fn install_script_is_single_line_and_pins_the_same_argv() {
        let script = platform::install_script("someone", "/etc/sudoers.d/probe", "/Library/LaunchDaemons/probe.plist");
        assert!(!script.contains('\n'), "AppleScript string literals cannot contain raw newlines");
        assert!(script.contains(r"printf '%s\n'"), "multi-line files are emitted via printf, not a heredoc");
        // The grant and the runtime toggle must agree character for character.
        assert!(script.contains("/usr/bin/pmset -a disablesleep 0, /usr/bin/pmset -a disablesleep 1"));
        assert!(script.contains("visudo -cf"), "the rule is validated before it is installed");
    }

    #[test]
    fn off_never_arms_by_itself() {
        let s = sample(LidAwakeMode::Off, true, 0);
        assert!(!s.wants_arm(None), "a busy agent alone must not arm under Off");
    }

    #[test]
    fn start_now_overrides_every_policy_including_off() {
        // "Start now" is an explicit request, so it holds even with no policy
        // set and no agent working — that's the whole point of the button.
        let s = sample(LidAwakeMode::Off, false, 0);
        assert!(s.wants_arm(Some(LEASE as i64)));
        for mode in [LidAwakeMode::Off, LidAwakeMode::OnBattery, LidAwakeMode::Always] {
            let mut idle_on_ac = sample(mode, false, 0);
            idle_on_ac.on_battery = false;
            assert!(idle_on_ac.wants_arm(Some(LEASE as i64)), "{mode:?} must honor an explicit request");
        }
    }

    #[test]
    fn a_lapsed_start_now_releases_at_once_under_off() {
        // The release grace only makes sense for the busy-driven policies. Under
        // Off, keying off `last_busy_at` would keep holding for as long as any
        // agent happened to be working — long past the window the user asked for.
        let s = sample(LidAwakeMode::Off, true, LEASE as i64);
        assert!(s.wants_release(None, LEASE as i64, Some(LEASE as i64)), "expired request ends immediately");
    }

    #[test]
    fn always_arms_on_any_power_source() {
        let mut s = sample(LidAwakeMode::Always, true, 0);
        assert!(s.wants_arm(None));
        s.on_battery = false;
        assert!(s.wants_arm(None), "AC-powered work still arms in Always");
    }

    #[test]
    fn on_battery_mode_skips_mains_power() {
        let mut s = sample(LidAwakeMode::OnBattery, true, 0);
        assert!(s.wants_arm(None));
        s.on_battery = false;
        assert!(!s.wants_arm(None), "docked on AC is not about to be carried off");
    }

    #[test]
    fn automatic_modes_need_a_busy_session() {
        for mode in [LidAwakeMode::OnBattery, LidAwakeMode::Always] {
            assert!(!sample(mode, false, 0).wants_arm(None));
        }
    }

    #[test]
    fn an_unused_start_now_window_lapses() {
        let s = sample(LidAwakeMode::Off, false, 0);
        assert!(s.wants_arm(Some(LEASE as i64)));
        assert!(!s.wants_arm(None), "no outstanding request");
        let late = sample(LidAwakeMode::Off, false, LEASE as i64 + 1);
        assert!(!late.wants_arm(Some(LEASE as i64)), "a request whose lid never shut lapses");
    }

    #[test]
    fn a_start_now_hold_gets_a_full_window_from_the_lid_close() {
        // "Start now" at T+0, lid shut at T+14min. `evaluate` re-anchors the
        // request to the close, so the hold runs to T+29 rather than expiring at
        // T+15 — the same full window the policy modes get.
        let reanchored = Some(14 * MIN + LEASE as i64);
        let s = sample(LidAwakeMode::Off, false, 15 * MIN + 1);
        assert!(s.wants_arm(reanchored));
        assert!(!s.wants_release(Some(14 * MIN), 0, reanchored));

        let expired = sample(LidAwakeMode::Off, false, 29 * MIN);
        assert!(expired.wants_release(Some(14 * MIN), 0, reanchored), "released 15 min after the close");
    }

    #[test]
    fn lease_is_anchored_to_the_lid_not_the_work() {
        // The bug this design exists to avoid: work starts at T+0, the lid shuts
        // at T+13min. A work-anchored lease would expire two minutes later.
        let s = sample(LidAwakeMode::Always, true, 13 * MIN);
        assert!(!s.wants_release(Some(13 * MIN), 13 * MIN, None), "countdown starts at the lid close");

        let near_end = sample(LidAwakeMode::Always, true, 27 * MIN);
        assert!(
            !near_end.wants_release(Some(13 * MIN), 27 * MIN, None),
            "still inside the full window measured from the close"
        );

        let expired = sample(LidAwakeMode::Always, true, 28 * MIN);
        assert!(expired.wants_release(Some(13 * MIN), 28 * MIN, None), "15 min after the close");
    }

    #[test]
    fn lid_open_work_never_expires() {
        // Hours of lid-open work must not trip the lease — there is no lid close
        // to measure from.
        let s = sample(LidAwakeMode::Always, true, 6 * 60 * MIN);
        assert!(!s.wants_release(None, 6 * 60 * MIN, None));
    }

    #[test]
    fn release_grace_absorbs_the_between_turns_flap() {
        // Agent goes Done for 30s then Working again: releasing on the first
        // Done would sleep the Mac out from under the next turn.
        let idle = sample(LidAwakeMode::Always, false, 30_000);
        assert!(!idle.wants_release(None, 0, None), "inside the grace");
        let settled = sample(LidAwakeMode::Always, false, GRACE as i64);
        assert!(settled.wants_release(None, 0, None), "grace elapsed with nothing busy");
    }

    #[test]
    fn battery_floor_blocks_arming_and_forces_release() {
        let mut s = sample(LidAwakeMode::Always, true, 0);
        s.battery_pct = Some(19);
        assert!(!s.wants_arm(None));
        // …and overrides even an explicit "Start now", since `disablesleep`
        // suppresses the low-battery emergency sleep that would otherwise save
        // a closed laptop from running itself flat.
        let mut requested = sample(LidAwakeMode::Off, false, 0);
        requested.battery_pct = Some(5);
        assert!(!requested.wants_arm(Some(LEASE as i64)));
        assert!(requested.wants_release(Some(0), 0, Some(LEASE as i64)));
    }

    #[test]
    fn unknown_or_unfloored_battery_does_not_block() {
        let mut none = sample(LidAwakeMode::Always, true, 0);
        none.battery_pct = None; // a desktop Mac
        assert!(none.wants_arm(None));
        let mut unfloored = sample(LidAwakeMode::Always, true, 0);
        unfloored.battery_pct = Some(1);
        unfloored.floor_pct = 0;
        assert!(unfloored.wants_arm(None));
    }

    #[test]
    fn zero_lease_disables_the_feature() {
        let mut s = sample(LidAwakeMode::Always, true, 0);
        s.lease_ms = 0;
        assert!(!s.wants_arm(None));
    }

    #[test]
    fn waiting_counts_as_busy_but_blocked_does_not() {
        for st in [Status::Working, Status::Waiting] {
            assert!(is_busy(st), "{st:?} is real work that sleeping would suspend");
        }
        // Blocked is parked on the user — nothing progresses while asleep, so it
        // must not hold the veto (and with it, thermal safety sleep) open.
        for st in [Status::Idle, Status::Blocked, Status::Done, Status::Error] {
            assert!(!is_busy(st), "{st:?} should not hold the Mac awake");
        }
    }
}
