use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::config::{ConfigState, StateNotify};
use crate::custom_names::CustomNamesStore;
use crate::state::{AgentSession, AppState, DialogRole, Status};
use crate::telegram::{SyncOutcome, TelegramNotifier};
use crate::usage_limits::UsageLimitsState;

/// Upper bound on the reading-time delay [`reading_time_ms`] can add to a
/// notification window, so an enormous message can't defer an actionable ping
/// indefinitely. Sits comfortably above a realistic full-page answer (~3.5k
/// chars at the default pace) so it bounds runaway walls of text without
/// clipping the very case the reading delay exists to cover.
const READING_CAP_MS: u64 = 360_000;

#[derive(Clone, Debug)]
pub struct Outstanding {
    pub handle: String,
    pub for_status: Status,
}

/// A failed notification send, tagged with whether the message might already
/// have reached the channel — the signal [`reconcile`]'s retry backoff keys on
/// to keep a network outage from multiplying duplicate pings.
#[derive(Debug)]
pub struct SendError {
    /// A post-connect read timeout: the channel may have received the message
    /// and only the ACK was lost, so retrying immediately risks a duplicate the
    /// channel gives no way to dedupe. `false` for a connect error / API
    /// rejection / disabled, where the message definitely didn't land and a
    /// prompt retry is duplicate-free.
    pub maybe_delivered: bool,
    pub source: anyhow::Error,
}

/// After a send that *might* already have been delivered (a read timeout — see
/// [`SendError::maybe_delivered`]), how long to hold before retrying that
/// session's ping. Immediately retrying an ambiguous timeout is what turned a
/// ~4-min Telegram outage into six duplicate pings: each attempt reached
/// Telegram and delivered, but the lost ACK read as failure and the next tick
/// resent. Holding collapses a sustained outage to ~1 retry while staying
/// at-least-once (a genuinely-undelivered ping still lands, just later). The
/// value is generous because a maybe-delivered ping most likely already
/// arrived, so waiting costs nothing in the common case — only a true loss
/// pays it, and even then a `done` ping tolerates the delay.
const UNCERTAIN_RETRY_BACKOFF_MS: i64 = 5 * 60 * 1000;

/// A per-session retry hold set after a maybe-delivered send failure: don't
/// retry this session's ping before `until_ms`. `for_status` scopes the hold to
/// the state that failed, so a *new* actionable state (e.g. Done→Blocked) pings
/// promptly instead of inheriting the previous state's backoff.
#[derive(Clone, Debug)]
pub struct RetryHold {
    pub until_ms: i64,
    pub for_status: Status,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn channel_name(&self) -> &'static str;
    fn is_enabled(&self) -> bool;
    fn state_rules(&self) -> HashMap<String, StateNotify>;
    async fn send(&self, session: &AgentSession) -> Result<String, SendError>;
    async fn dismiss(&self, handle: &str) -> anyhow::Result<()>;
}

pub fn status_key(s: Status) -> &'static str {
    match s {
        Status::Idle => "idle",
        Status::Working => "working",
        // `Waiting` (background agents) is passive — no config key, so it never
        // fires (like `working`/`idle`).
        Status::Waiting => "waiting",
        Status::Blocked => "blocked",
        Status::Done => "done",
        Status::Error => "error",
    }
}

/// The text the notification shows under the status line — mirroring the
/// frontend's `primaryText` (`src/lib/types.ts`; keep the two in sync). For a
/// row the user must act on (`blocked`/`error`) it's the current `label` (the
/// question / approval request); otherwise it's the original task
/// (`original_prompt`), falling back to `label`. Without this a `done` row that
/// was previously `blocked` would carry its stale `Blocked` label (the
/// `Stop`→Done event has no label, so `label_policy` preserves the old one), so
/// the message read e.g. "done\nneeds approval: tool" while the dashboard row
/// already showed the finished task.
fn primary_text(session: &AgentSession) -> &str {
    match session.status {
        Status::Blocked | Status::Error => &session.label,
        _ => session.original_prompt.as_deref().unwrap_or(&session.label),
    }
}

pub fn build_message_text(session: &AgentSession) -> String {
    let status = status_key(session.status);
    let name = session.display_label();
    let text = primary_text(session);
    if text.trim().is_empty() {
        format!("[{}] {}", name, status)
    } else {
        format!("[{}] {}\n{}", name, status, text)
    }
}

/// Resolve a model's context window: exact key first, then the longest key
/// that is a prefix of the model name — so "claude-opus" covers every future
/// opus release without a config update. Mirrored by the frontend `windowFor`
/// in types.ts; keep the two in sync.
pub fn window_for(model: &str, window_tokens: &HashMap<String, u64>) -> Option<u64> {
    if let Some(max) = window_tokens.get(model).copied().filter(|m| *m > 0) {
        return Some(max);
    }
    window_tokens.iter().filter(|(k, v)| model.starts_with(k.as_str()) && **v > 0).max_by_key(|(k, _)| k.len()).map(|(_, v)| *v)
}

/// A session's context usage as a percent of its model's window, mirroring
/// the frontend's `tokenColor` math. `None` when the session has no token
/// count, no model, or no configured window for that model.
pub fn context_percent(session: &AgentSession, window_tokens: &HashMap<String, u64>) -> Option<f32> {
    let tokens = session.input_tokens?;
    let max = window_for(session.model.as_ref()?, window_tokens)?;
    Some(tokens as f32 / max as f32 * 100.0)
}

fn tokens_k(n: u64) -> String {
    format!("{}k", n.div_ceil(1000))
}

/// Telegram text for a context-usage alert, e.g. `[proj] context 72% (144k/200k)`.
/// `None` when the percent can't be computed (same conditions as [`context_percent`]).
pub fn build_context_message(session: &AgentSession, window_tokens: &HashMap<String, u64>) -> Option<String> {
    let pct = context_percent(session, window_tokens)?;
    let tokens = session.input_tokens?;
    let max = window_for(session.model.as_ref()?, window_tokens)?;
    Some(format!("[{}] context {}% ({}/{})", session.display_label(), pct.round() as u32, tokens_k(tokens), tokens_k(max)))
}

/// Reconcile context-usage alerts against the currently-over set, mirroring the
/// per-state notification lifecycle: an alert is *sent* when a session first
/// crosses `threshold_percent` of its context window, and *dismissed* (the
/// Telegram message deleted) once it's no longer over — because usage dropped
/// back below (a new task / `/clear`), the session vanished, or its window/model
/// became unknown. `outstanding` maps a session id to the handle of its live
/// alert message. Returns `(to_dismiss, to_send)`: ids whose message should be
/// deleted (still present in `outstanding` — the caller removes them), and
/// sessions a fresh alert should be sent for. `threshold_percent <= 0` disables
/// the feature, so everything outstanding is dismissed.
pub fn context_reconcile<'a>(
    threshold_percent: f32,
    sessions: &'a [AgentSession],
    window_tokens: &HashMap<String, u64>,
    outstanding: &HashMap<String, String>,
) -> (Vec<String>, Vec<&'a AgentSession>) {
    let is_over = |s: &AgentSession| threshold_percent > 0.0 && context_percent(s, window_tokens).is_some_and(|p| p >= threshold_percent);
    let over_ids: HashSet<&str> = sessions.iter().filter(|s| is_over(s)).map(|s| s.id.as_str()).collect();
    let to_dismiss: Vec<String> = outstanding.keys().filter(|id| !over_ids.contains(id.as_str())).cloned().collect();
    let to_send: Vec<&AgentSession> = sessions.iter().filter(|s| is_over(s) && !outstanding.contains_key(&s.id)).collect();
    (to_dismiss, to_send)
}

/// The on-screen text whose length sets how long the user needs to *read* before
/// they could react — the basis for the reading-time delay ([`reading_time_ms`]).
///
/// - `blocked` / `done`: the full final assistant turn (the multi-paragraph
///   answer or completion summary the user actually reads), taken from the last
///   `Assistant` dialog entry. Falls back to `label` only when there is no
///   assistant entry yet — a `PreToolUse` / permission `blocked` row, where
///   `label` *is* the question. (A `Stop`→`Blocked` `label` is the fixed string
///   `"has a question"`, so the dialog text is preferred over it.)
/// - `error`: `label` (the short failure kind). A `StopFailure` turn emits no
///   assistant text, so the last dialog entry is the *prior* successful turn —
///   scaling off that would delay an actionable error ping by an irrelevant
///   length, so `error` reads its own short label and stays snappy.
fn read_burden_text(session: &AgentSession) -> &str {
    if session.status == Status::Error {
        return &session.label;
    }
    session
        .dialog
        .iter()
        .rev()
        .find(|e| e.role == DialogRole::Assistant)
        .map(|e| e.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&session.label)
}

/// Milliseconds to allow for *reading* `text` before a notification is due, at
/// `speed_cps` characters/second, clamped to `cap_ms`. Added to both the AFK and
/// reaction windows (see [`fire_reason`]) so a present user isn't pinged while
/// still reading a long message. `speed_cps == 0` disables the scaling and an
/// empty message reads as `0` — both reproduce the fixed-window behavior.
pub fn reading_time_ms(text: &str, speed_cps: u64, cap_ms: u64) -> u64 {
    if speed_cps == 0 {
        return 0;
    }
    let chars = text.trim().chars().count() as u64;
    (chars.saturating_mul(1000) / speed_cps).min(cap_ms)
}

/// Decide whether a state in `rule` is due to fire, given how long it has been
/// in that state, the system-wide input-idle time, and the reading-time budget
/// for its message ([`reading_time_ms`]). Returns the reason it fired (for
/// logging), or `None` if neither criterion is met.
///
/// - **AFK** (`afk_window_ms`): the user is idle past the window *plus* the
///   reading budget *and* has not touched the machine since the state began
///   (`idle >= time_in_state`, the "saw it" guard). Skipped when idle is unknown
///   (presence can't be proven). Adding `reading_ms` is what stops a present,
///   silently-*reading* user — whose idle climbs in lockstep with time-in-state
///   and so always satisfies the guard — from being pinged mid-read.
/// - **Reaction** (`reaction_window_ms`): the state has outlasted the backstop
///   *plus* the reading budget, regardless of presence.
///
/// Whichever trips first wins; AFK lets a notification fire sooner once the
/// user has stepped away. `reading_ms` is *added* (not a floor): the base window
/// is the length-independent notice/decide/act budget, `reading_ms` the
/// length-dependent time to consume the content, and they run in sequence. The
/// `afk > 0` / `r > 0` gates run before the addition, so a disabled window is
/// never resurrected by a long message.
/// Whether a state rule is configured to notify at all — at least one of its
/// two windows is set to a positive duration. A rule with both windows unset or
/// zeroed never fires (the user's way to silence a state), so high alert honors
/// it too. Mirrors the `afk > 0` / `r > 0` gates inside [`fire_reason`].
pub fn rule_notifies(rule: &StateNotify) -> bool {
    rule.afk_window_ms.unwrap_or(0) > 0 || rule.reaction_window_ms.unwrap_or(0) > 0
}

pub fn fire_reason(rule: &StateNotify, time_in_state_ms: u64, idle_ms: Option<u64>, reading_ms: u64) -> Option<&'static str> {
    let afk_due = matches!((rule.afk_window_ms, idle_ms), (Some(afk), Some(idle)) if afk > 0 && idle >= afk.saturating_add(reading_ms) && idle >= time_in_state_ms);
    if afk_due {
        return Some("afk");
    }
    let reaction_due = matches!(rule.reaction_window_ms, Some(r) if r > 0 && time_in_state_ms >= r.saturating_add(reading_ms));
    reaction_due.then_some("reaction")
}

/// Revoke a batch of outstanding notifications and forget them: for each id,
/// delete its message via `Notifier::dismiss` (logging but tolerating failure)
/// and drop it from `outstanding`. Shared by both reconcilers — the per-state
/// one ([`reconcile`]) and the context-usage one ([`context_reconcile`]) — which
/// differ only in *which* entries go stale and in how the handle is stored
/// (hence the `handle_of` accessor over a generic value type).
pub async fn dismiss_and_forget<V>(
    notifier: &dyn Notifier,
    outstanding: &mut HashMap<String, V>,
    ids: impl IntoIterator<Item = String>,
    handle_of: impl Fn(&V) -> &str,
) {
    for id in ids {
        let Some(entry) = outstanding.remove(&id) else { continue };
        let handle = handle_of(&entry).to_string();
        if let Err(e) = notifier.dismiss(&handle).await {
            tracing::debug!(channel = notifier.channel_name(), id = %id, handle = %handle, ?e, "dismiss failed");
        }
    }
}

// ---- usage-limit reset detection --------------------------------------------

/// Which usage window reset. `label` is the terse Telegram wording; `key` is the
/// greppable `bucket` field in the decision log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitWindow {
    FiveHour,
    SevenDay,
}

impl LimitWindow {
    fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "7d",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::FiveHour => "five_hour",
            Self::SevenDay => "seven_day",
        }
    }
}

/// A detected reset of one usage window, carrying the peak percent the
/// just-ended window reached (its high-water mark).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimitReset {
    pub window: LimitWindow,
    pub peak_pct: f32,
}

/// A reset is only registered when `resets_at` jumps forward by more than this.
/// It sits far above the ±1min jitter the fixed 5h window's `resets_at` shows
/// between polls (see the `usage_five_hour_resets_at_jitter` memory) and far
/// below any real reset jump — the smallest is a full window minus how early it
/// fired, ≥ ~3h for 5h and days for 7d — so it cleanly separates jitter from a
/// genuine window turnover.
pub const RESET_JUMP_MARGIN_MS: i64 = 10 * 60 * 1000;

/// Slack around a window's scheduled `resets_at` within which the reset counts as
/// *arrived*. On a genuine reset the reset poll lands right at (or just after) the
/// scheduled time, while collateral zeroing (the sibling bucket cratering this one
/// when *it* resets) strikes while this window's `resets_at` is still hours or
/// days ahead — so a couple of minutes of slack (covering poll cadence + the ±1min
/// `resets_at` jitter) cleanly separates the two.
pub const RESET_ARRIVAL_SLACK_MS: i64 = 2 * 60 * 1000;

/// Per-window reset tracker. A reset is credited on the earlier of two signals:
/// the prompt path — this window's `resets_at` clears to null with its percentage
/// cleared (0 or null) *and its scheduled reset time has arrived* — and the
/// fallback path —
/// `resets_at` jumping forward past the just-ended window (for coarse polls that
/// skip the null transient, or early rollovers the arrival gate declines). The
/// arrival gate is what makes the prompt path safe: a 5h reset transiently zeroes
/// *both* buckets' percentages (and can null *both* `resets_at`s) for up to ~40min
/// (observed in real data), so a bare null-drop can't tell a real reset from
/// collateral — but the collateral bucket's own scheduled reset is still far in
/// the future, so it fails the gate and falls through to the immune forward-jump
/// path. `peak_pct` (a running max) rides out the transient dips so the pre-reset
/// high-water mark is the value reported.
#[derive(Default)]
struct WindowReset {
    /// Last non-null `resets_at` seen (this window's scheduled boundary); `None`
    /// until the first observation (seeded silently, so a fresh tracker never
    /// fires on its very first poll).
    resets_at: Option<i64>,
    peak_pct: f32,
    /// Set once we've prompt-fired a reset off the `resets_at`→null transient and
    /// are waiting for `resets_at` to recover to its new value; the recovery poll
    /// (a forward jump) is then consumed silently so the reset isn't reported
    /// twice.
    awaiting_recovery: bool,
}

impl WindowReset {
    /// Fold a poll's percentage into the running high-water mark.
    fn track_peak(&mut self, pct: Option<f32>) {
        if let Some(p) = pct {
            self.peak_pct = self.peak_pct.max(p);
        }
    }

    /// Fold one poll's `(percent, resets_at)` for this window into the tracker,
    /// with `now_ms` the poll's wall-clock (the usage snapshot's `updated`).
    /// Returns `Some(peak)` — the just-ended window's high-water mark — exactly
    /// when a reset is detected, else `None`.
    fn observe(
        &mut self,
        pct: Option<f32>,
        resets_at: Option<i64>,
        now_ms: i64,
        margin_ms: i64,
    ) -> Option<f32> {
        match resets_at {
            // `resets_at` cleared: this bucket is in a reset transient — either its
            // own real reset, or collateral zeroing from the sibling window's reset.
            None => {
                if self.awaiting_recovery {
                    // Already reported this reset; keep tracking the new window.
                    self.track_peak(pct);
                    return None;
                }
                // Prompt path: a cleared `resets_at` at/after this window's *own*
                // scheduled reset is itself the reset marker — fire whether the
                // reset poll's percentage came back as 0 or as null (the API returns
                // "0/null" in the transient). The arrival gate rejects collateral
                // zeroing (sibling reset; our `resets_at` still far ahead) and early
                // rollovers (deferred to the forward-jump path). Only a concrete
                // *positive* percentage contradicts a reset — fall through on that.
                let arrived = self
                    .resets_at
                    .is_some_and(|sched| now_ms + RESET_ARRIVAL_SLACK_MS >= sched);
                if arrived && pct.map_or(true, |p| p <= 0.0) {
                    let peak = self.peak_pct; // the ended window's high-water mark
                    self.awaiting_recovery = true;
                    self.peak_pct = 0.0; // new window starts here
                    return Some(peak);
                }
                // Collateral / early / pre-seed null: just fold the percentage in.
                self.track_peak(pct);
                None
            }
            Some(r) => {
                if self.awaiting_recovery {
                    // Recovery poll after a prompt-fired reset: adopt the new
                    // window's scheduled time and consume the jump silently.
                    self.resets_at = Some(r);
                    self.awaiting_recovery = false;
                    self.track_peak(pct);
                    return None;
                }
                // Forward-jump fallback: a coarse poll skipped the `(0, null)`
                // transient, or an early rollover didn't pass the arrival gate,
                // landing directly on the jumped `resets_at`. Its percentage
                // belongs to the new window, so it must not inflate the ended
                // window's peak.
                if let Some(prev) = self.resets_at {
                    if r > prev + margin_ms {
                        let peak = self.peak_pct;
                        self.resets_at = Some(r);
                        self.peak_pct = pct.unwrap_or(0.0); // new window starts here
                        return Some(peak);
                    }
                }
                // First observation seeds silently; jitter keeps the same window.
                self.resets_at = Some(r);
                self.track_peak(pct);
                None
            }
        }
    }
}

/// Tracks 5h + 7d window resets across polls. Loop-local state in the
/// notification manager; the detection is pure so it unit-tests without a poll
/// loop.
#[derive(Default)]
pub struct ResetTracker {
    five_hour: WindowReset,
    seven_day: WindowReset,
}

impl ResetTracker {
    /// Fold one usage poll (each bucket's `(percent, resets_at)`) into the
    /// tracker, returning any windows that just reset with their peak percent.
    pub fn observe(
        &mut self,
        five_hour: (Option<f32>, Option<i64>),
        seven_day: (Option<f32>, Option<i64>),
        now_ms: i64,
        margin_ms: i64,
    ) -> Vec<LimitReset> {
        let mut out = Vec::new();
        if let Some(peak) = self.five_hour.observe(five_hour.0, five_hour.1, now_ms, margin_ms) {
            out.push(LimitReset { window: LimitWindow::FiveHour, peak_pct: peak });
        }
        if let Some(peak) = self.seven_day.observe(seven_day.0, seven_day.1, now_ms, margin_ms) {
            out.push(LimitReset { window: LimitWindow::SevenDay, peak_pct: peak });
        }
        out
    }
}

/// Terse Telegram text for a window reset, e.g. `5h limit reset (was 96%)`.
/// Account-wide, so unlike the per-session messages it carries no `[name]`.
pub fn build_limit_reset_message(reset: LimitReset) -> String {
    format!("{} limit reset (was {}%)", reset.window.label(), reset.peak_pct.round() as i64)
}

pub async fn reconcile(
    notifier: &dyn Notifier,
    sessions: &[AgentSession],
    outstanding: &mut HashMap<String, Outstanding>,
    retry_backoff: &mut HashMap<String, RetryHold>,
    now_ms: i64,
    idle_ms: Option<u64>,
    reading_speed_cps: u64,
    high_alert: bool,
) {
    let rules = notifier.state_rules();

    let stale: Vec<String> = outstanding
        .iter()
        .filter(|(id, o)| {
            sessions
                .iter()
                .find(|s| &s.id == *id)
                .map_or(true, |s| s.status != o.for_status)
        })
        .map(|(k, _)| k.clone())
        .collect();
    dismiss_and_forget(notifier, outstanding, stale, |o| o.handle.as_str()).await;

    // Drop retry holds whose session vanished or changed state: a hold only
    // scopes the state that failed to send, so a new state (or a resolved one)
    // must be free to ping without waiting out the old state's backoff.
    retry_backoff.retain(|id, h| sessions.iter().any(|s| &s.id == id && s.status == h.for_status));

    for s in sessions {
        if outstanding.contains_key(&s.id) {
            continue;
        }
        let key = status_key(s.status);
        let Some(rule) = rules.get(key) else { continue };
        let time_in_state = (now_ms - s.state_entered_at).max(0) as u64;
        // High alert short-circuits the windows entirely: any state that's
        // configured to notify at all (at least one positive window) fires the
        // instant it's entered, with no reading-time budget. A state disabled by
        // zeroing both windows still stays silent — high alert doesn't resurrect
        // it. Otherwise fall through to the normal AFK/reaction decision.
        let (reason, reading_ms) = if high_alert && rule_notifies(rule) {
            (Some("high_alert"), 0)
        } else {
            let reading_ms = reading_time_ms(read_burden_text(s), reading_speed_cps, READING_CAP_MS);
            (fire_reason(rule, time_in_state, idle_ms, reading_ms), reading_ms)
        };
        let Some(reason) = reason else { continue };
        // Hold off retrying a ping whose prior send may already have landed (a
        // read timeout). The `retain` above guarantees any surviving hold is for
        // this same status, so a bare deadline check is enough.
        if retry_backoff.get(&s.id).is_some_and(|h| now_ms < h.until_ms) {
            continue;
        }
        match notifier.send(s).await {
            Ok(handle) => {
                tracing::debug!(
                    channel = notifier.channel_name(),
                    id = %s.id,
                    status = key,
                    reason,
                    reading_ms,
                    "notification fired"
                );
                outstanding.insert(
                    s.id.clone(),
                    Outstanding { handle, for_status: s.status },
                );
                retry_backoff.remove(&s.id);
            }
            // A maybe-delivered failure (read timeout) backs off before the next
            // attempt so a sustained outage can't multiply duplicate pings; a
            // definitely-not-delivered failure (connect error / API rejection)
            // clears any hold and lets the next tick retry promptly.
            Err(SendError { maybe_delivered: true, source }) => {
                retry_backoff.insert(s.id.clone(), RetryHold { until_ms: now_ms + UNCERTAIN_RETRY_BACKOFF_MS, for_status: s.status });
                tracing::warn!(
                    channel = notifier.channel_name(),
                    id = %s.id,
                    status = key,
                    backoff_ms = UNCERTAIN_RETRY_BACKOFF_MS,
                    error = %source,
                    "send may have been delivered; backing off retry"
                );
            }
            Err(SendError { maybe_delivered: false, source }) => {
                retry_backoff.remove(&s.id);
                tracing::warn!(
                    channel = notifier.channel_name(),
                    id = %s.id,
                    error = %source,
                    "send failed"
                );
            }
        }
    }
}

pub struct NotificationManager;

impl NotificationManager {
    pub fn spawn(app: AppHandle) {
        tauri::async_runtime::spawn(async move {
            let telegram = Arc::new(TelegramNotifier::new());
            let mut outstanding: HashMap<String, Outstanding> = HashMap::new();
            // Per-session retry holds after a maybe-delivered send failure, so a
            // network outage backs off instead of resending the same ping each
            // tick (see [`RetryHold`] / [`UNCERTAIN_RETRY_BACKOFF_MS`]).
            let mut retry_backoff: HashMap<String, RetryHold> = HashMap::new();
            // Live context-usage alerts: session id -> Telegram message handle,
            // so the message can be deleted once usage drops back below.
            let mut context_outstanding: HashMap<String, String> = HashMap::new();
            // Usage-limit reset detector + the last usage-poll `updated` we
            // processed, so each poll is folded in exactly once. `observe` fuses
            // detection with state advance (the jump can't be re-detected), so a
            // fired reset that fails to send is buffered here and retried each
            // tick rather than lost — mirroring how the other reconcilers recover
            // from a transient send failure (they simply don't record success).
            let mut reset_tracker = ResetTracker::default();
            let mut last_usage_updated: i64 = 0;
            let mut pending_limit_resets: Vec<LimitReset> = Vec::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            // First tick fires immediately; skip it so startup doesn't see
            // stale state before AppState is populated.
            ticker.tick().await;

            tracing::info!("notification manager started");

            loop {
                ticker.tick().await;

                let Some(cfg_state) = app.try_state::<ConfigState>() else { continue };
                let Some(app_state) = app.try_state::<AppState>() else { continue };
                let cfg = cfg_state.snapshot();
                // Local sessions only (notifications stay remote-blind), with the
                // custom-name overlay applied so a renamed agent's notification
                // text matches the dashboard. Same resolution as the emit-time
                // `resolved_snapshot`.
                let mut sessions = app_state.snapshot();
                if let Some(names) = app.try_state::<CustomNamesStore>() {
                    names.apply(&mut sessions);
                }

                let tg_cfg = cfg
                    .notifications
                    .as_ref()
                    .and_then(|n| n.telegram.as_ref());

                let outcome = telegram.sync_config(tg_cfg);
                if matches!(outcome, SyncOutcome::CredsChanged | SyncOutcome::Disabled) {
                    if !outstanding.is_empty() || !context_outstanding.is_empty() {
                        tracing::warn!(
                            channel = "telegram",
                            reason = ?outcome,
                            count = outstanding.len(),
                            context_count = context_outstanding.len(),
                            "credentials changed or disabled; dropping outstanding maps without deleting"
                        );
                        outstanding.clear();
                        context_outstanding.clear();
                    }
                    // Retry holds are handle-free (nothing to delete), but they
                    // predate the new credentials, so drop them too — a ping held
                    // under the old bot must re-evaluate under the new one.
                    retry_backoff.clear();
                    // Re-seed the reset detector so a window that reset while
                    // creds were absent doesn't fire a stale / frozen-peak ping
                    // on re-enable: `observe` is gated on `is_enabled()` below, so
                    // it can't stay seeded across a creds-cleared gap. A fresh
                    // tracker seeds the next poll silently (mirrors app restart).
                    // Drop buffered-but-unsent resets too — they predate the new
                    // credentials and mustn't be delivered under them.
                    reset_tracker = ResetTracker::default();
                    last_usage_updated = 0;
                    pending_limit_resets.clear();
                }

                if telegram.is_enabled() {
                    reconcile(
                        telegram.as_ref() as &dyn Notifier,
                        &sessions,
                        &mut outstanding,
                        &mut retry_backoff,
                        now_ms(),
                        crate::idle::idle_ms(),
                        tg_cfg.and_then(|c| c.reading_speed_cps).unwrap_or(0),
                        cfg.high_alert,
                    )
                    .await;

                    let threshold = tg_cfg.and_then(|c| c.context_alert_percent).unwrap_or(0.0);
                    let (to_dismiss, to_send) = context_reconcile(threshold, &sessions, &cfg.context_window_tokens, &context_outstanding);
                    // Delete alerts whose session dropped back below the threshold
                    // (or vanished) — same revoke path as per-state notifications.
                    // Logged per id (greppable `decision`) so the alert lifecycle is
                    // reconstructable from widget.jsonl like every other state
                    // decision; the reason distinguishes a usage drop (session still
                    // present) from a clear/end (session gone).
                    for id in &to_dismiss {
                        tracing::debug!(
                            channel = "telegram",
                            id = %id,
                            decision = "context_dismiss",
                            reason = if sessions.iter().any(|s| &s.id == id) {
                                "context dropped below threshold"
                            } else {
                                "session cleared or ended"
                            },
                            "context alert dismissed"
                        );
                    }
                    dismiss_and_forget(telegram.as_ref() as &dyn Notifier, &mut context_outstanding, to_dismiss, |h| h.as_str()).await;
                    for s in to_send {
                        let Some(text) = build_context_message(s, &cfg.context_window_tokens) else { continue };
                        match telegram.send_raw(&text).await {
                            // Track the handle so we can delete it when usage drops.
                            Ok(handle) => {
                                tracing::debug!(
                                    channel = "telegram",
                                    id = %s.id,
                                    decision = "context_alert",
                                    percent = context_percent(s, &cfg.context_window_tokens).unwrap_or(0.0),
                                    threshold,
                                    reason = "context usage crossed alert threshold",
                                    "context alert sent"
                                );
                                context_outstanding.insert(s.id.clone(), handle);
                            }
                            // Not tracked → the next tick retries this session.
                            Err(e) => tracing::warn!(channel = "telegram", id = %s.id, ?e, "context alert send failed"),
                        }
                    }

                    // Usage-limit reset pings. When the account-wide 5h/7d
                    // window resets after heavy use, fire a one-shot message —
                    // detected the instant the window's percentage drops to zero at
                    // its scheduled reset time, with a forward-`resets_at`-jump
                    // fallback for coarse polls / early rollovers (see
                    // `ResetTracker`). Processed once per poll (gated on `updated`
                    // advancing), with `updated` doubling as the poll's wall-clock
                    // for the reset-arrival check. A point event: buffered and
                    // retried until sent, never tracked for deletion. `observe`
                    // runs whenever creds are set (even with the feature off) so
                    // the tracker stays seeded and turning the threshold on
                    // mid-window can't false-fire on a stale window.
                    if let Some(usage) = app.try_state::<UsageLimitsState>().map(|s| s.snapshot()) {
                        if usage.updated != 0 && usage.updated != last_usage_updated {
                            last_usage_updated = usage.updated;
                            let five = (
                                usage.five_hour.as_ref().map(|b| b.utilization * 100.0),
                                usage.five_hour.as_ref().and_then(|b| b.resets_at),
                            );
                            let seven = (
                                usage.seven_day.as_ref().map(|b| b.utilization * 100.0),
                                usage.seven_day.as_ref().and_then(|b| b.resets_at),
                            );
                            let threshold = tg_cfg.and_then(|c| c.limit_reset_percent).unwrap_or(0.0);
                            for reset in reset_tracker.observe(five, seven, usage.updated, RESET_JUMP_MARGIN_MS) {
                                let fired = threshold > 0.0 && reset.peak_pct >= threshold;
                                tracing::debug!(
                                    channel = "telegram",
                                    decision = "limit_reset",
                                    bucket = reset.window.key(),
                                    peak_pct = reset.peak_pct,
                                    threshold,
                                    fired,
                                    reason = "usage window reset detected",
                                    "limit reset detected"
                                );
                                // Buffer rather than send inline: `observe` has
                                // already consumed the reset, so a failed send
                                // must be retried below, not dropped.
                                if fired {
                                    pending_limit_resets.push(reset);
                                }
                            }
                        }
                    }

                    // Deliver (and retry) buffered reset pings every tick,
                    // independent of the poll gate, until Telegram accepts them.
                    let mut still_pending = Vec::new();
                    for reset in pending_limit_resets.drain(..) {
                        if let Err(e) = telegram.send_raw(&build_limit_reset_message(reset)).await {
                            tracing::warn!(channel = "telegram", bucket = reset.window.key(), ?e, "limit reset send failed; will retry");
                            still_pending.push(reset);
                        }
                    }
                    pending_limit_resets = still_pending;
                }
            }
        });
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AgentSession, DialogEntry, DialogRole, Status};
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq)]
    enum Event {
        Send { id: String, for_status: Status, handle: String },
        Dismiss { handle: String },
    }

    struct Mock {
        rules: HashMap<String, StateNotify>,
        events: Mutex<Vec<Event>>,
        handle_counter: Mutex<u64>,
        /// `None` = sends succeed; `Some(maybe_delivered)` = every send fails,
        /// with that delivery certainty (drives the retry-backoff branch).
        send_err: Mutex<Option<bool>>,
    }

    impl Mock {
        /// Reaction-window-only rules, matching the pre-AFK threshold behavior.
        fn with(thresholds: &[(&str, u64)]) -> Arc<Self> {
            Self::with_rules(&thresholds.iter().map(|(k, v)| (*k, StateNotify { afk_window_ms: None, reaction_window_ms: Some(*v) })).collect::<Vec<_>>())
        }
        fn with_rules(rules: &[(&str, StateNotify)]) -> Arc<Self> {
            Arc::new(Self {
                rules: rules.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
                events: Mutex::new(vec![]),
                handle_counter: Mutex::new(0),
                send_err: Mutex::new(None),
            })
        }
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Notifier for Mock {
        fn channel_name(&self) -> &'static str { "mock" }
        fn is_enabled(&self) -> bool { true }
        fn state_rules(&self) -> HashMap<String, StateNotify> { self.rules.clone() }
        async fn send(&self, session: &AgentSession) -> Result<String, SendError> {
            if let Some(maybe_delivered) = *self.send_err.lock().unwrap() {
                return Err(SendError { maybe_delivered, source: anyhow::anyhow!("boom") });
            }
            let mut c = self.handle_counter.lock().unwrap();
            *c += 1;
            let handle = format!("h{}", *c);
            self.events.lock().unwrap().push(Event::Send {
                id: session.id.clone(),
                for_status: session.status,
                handle: handle.clone(),
            });
            Ok(handle)
        }
        async fn dismiss(&self, handle: &str) -> anyhow::Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(Event::Dismiss { handle: handle.to_string() });
            Ok(())
        }
    }

    fn session(id: &str, status: Status, state_entered_at: i64) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "test".to_string(),
            model: None,
            input_tokens: None,
            updated: state_entered_at,
            state_entered_at,
            working_accumulated_ms: 0,
            display_name: None,
            origin: None,
        }
    }

    /// A session carrying a final assistant message of `assistant_text` — the
    /// text `read_burden_text` measures for the reading-time delay.
    fn session_with_message(id: &str, status: Status, state_entered_at: i64, assistant_text: &str) -> AgentSession {
        let mut s = session(id, status, state_entered_at);
        s.dialog.push(DialogEntry {
            role: DialogRole::Assistant,
            text: assistant_text.to_string(),
            timestamp: state_entered_at,
            status,
            task_start: false,
        });
        s
    }

    #[tokio::test]
    async fn sends_when_threshold_elapsed_and_no_outstanding() {
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 60_000, None, 0, false).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out["s1"].for_status, Status::Blocked);
        assert_eq!(out["s1"].handle, "h1");
        assert!(matches!(m.events()[0], Event::Send { .. }));
    }

    #[tokio::test]
    async fn does_not_send_before_threshold() {
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 59_999, None, 0, false).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn noop_when_outstanding_matches_current_state() {
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h1".into(), for_status: Status::Blocked, },
        );
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 120_000, None, 0, false).await;
        assert_eq!(out.len(), 1);
        assert!(m.events().is_empty(), "no events when nothing changes");
    }

    #[tokio::test]
    async fn dismisses_when_session_transitions_to_different_state() {
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h9".into(), for_status: Status::Blocked, },
        );
        let sessions = vec![session("s1", Status::Working, 100_000)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 120_000, None, 0, false).await;
        assert!(out.is_empty());
        assert_eq!(m.events(), vec![Event::Dismiss { handle: "h9".into() }]);
    }

    #[tokio::test]
    async fn dismisses_when_session_vanishes_from_snapshot() {
        // This is the "user clicked × on the widget row" path — session
        // disappears entirely from AppState::snapshot().
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h7".into(), for_status: Status::Blocked, },
        );
        let sessions: Vec<AgentSession> = vec![];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 120_000, None, 0, false).await;
        assert!(out.is_empty());
        assert_eq!(m.events(), vec![Event::Dismiss { handle: "h7".into() }]);
    }

    #[tokio::test]
    async fn session_vanishes_mid_threshold_is_noop() {
        // User clicks × 30s into a 60s threshold; no outstanding exists yet.
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        let sessions: Vec<AgentSession> = vec![];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 30_000, None, 0, false).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn threshold_zero_means_silent() {
        let m = Mock::with(&[("blocked", 0)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 1_000_000, None, 0, false).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn missing_threshold_key_means_silent() {
        let m = Mock::with(&[("error", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 1_000_000, None, 0, false).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn send_failure_leaves_no_outstanding_so_next_tick_retries() {
        // A definitely-not-delivered failure (connect error / API rejection).
        let m = Mock::with(&[("blocked", 60_000)]);
        *m.send_err.lock().unwrap() = Some(false);
        let mut out = HashMap::new();
        let mut backoff = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut backoff, 60_000, None, 0, false).await;
        assert!(out.is_empty(), "failed send must not populate outstanding");
        assert!(backoff.is_empty(), "not-delivered failure sets no hold → next tick retries promptly");
    }

    #[tokio::test]
    async fn maybe_delivered_failure_backs_off_then_retries_after_window() {
        // A read timeout: the ping may already have landed, so the next tick must
        // NOT resend immediately — it holds for the backoff window, then retries.
        let m = Mock::with(&[("done", 60_000)]);
        *m.send_err.lock().unwrap() = Some(true);
        let mut out = HashMap::new();
        let mut backoff = HashMap::new();
        let sessions = vec![session("s1", Status::Done, 0)];
        // Window elapsed → attempt; the send times out (maybe delivered).
        reconcile(m.as_ref(), &sessions, &mut out, &mut backoff, 60_000, None, 0, false).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty(), "a failed send emits no Send event");
        assert!(matches!(backoff.get("s1"), Some(h) if h.for_status == Status::Done), "maybe-delivered failure sets a retry hold");
        // Channel recovers, but we're still inside the hold → no retry.
        *m.send_err.lock().unwrap() = None;
        reconcile(m.as_ref(), &sessions, &mut out, &mut backoff, 60_000 + 1_000, None, 0, false).await;
        assert!(m.events().is_empty(), "no retry while the hold is active");
        assert!(out.is_empty());
        // Past the hold → retries and succeeds, clearing the hold.
        reconcile(m.as_ref(), &sessions, &mut out, &mut backoff, 60_000 + UNCERTAIN_RETRY_BACKOFF_MS + 1, None, 0, false).await;
        assert_eq!(out.len(), 1, "retries once the backoff window elapses");
        assert!(!backoff.contains_key("s1"), "a successful send clears the hold");
    }

    #[tokio::test]
    async fn retry_hold_cleared_when_state_changes_so_new_state_pings_promptly() {
        // A hold is scoped to the state that failed; a *new* actionable state
        // must ping at once rather than inheriting the old state's backoff.
        let m = Mock::with(&[("done", 60_000), ("blocked", 60_000)]);
        *m.send_err.lock().unwrap() = Some(true);
        let mut out = HashMap::new();
        let mut backoff = HashMap::new();
        let done = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &done, &mut out, &mut backoff, 60_000, None, 0, false).await;
        assert!(matches!(backoff.get("s1"), Some(h) if h.for_status == Status::Done));
        // Same session is now Blocked; channel recovered.
        *m.send_err.lock().unwrap() = None;
        let blocked = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &blocked, &mut out, &mut backoff, 60_000, None, 0, false).await;
        assert_eq!(out.len(), 1, "the new Blocked state pings promptly");
        assert_eq!(out["s1"].for_status, Status::Blocked);
    }

    // -------- fire_reason (per-state AFK + reaction rule) --------
    // These pass reading_ms = 0 (scaling disabled), so they pin the base-window
    // behavior; the reading-time cases live in the section further below.

    const AFK_ONLY: StateNotify = StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: None };
    const BOTH: StateNotify = StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: Some(120_000) };

    #[test]
    fn afk_fires_when_idle_past_window_and_no_input_since_state_began() {
        // 10s into the state, user idle 70s — away the whole time, fires.
        assert_eq!(fire_reason(&AFK_ONLY, 10_000, Some(70_000), 0), Some("afk"));
    }

    #[test]
    fn afk_suppressed_when_user_active_since_state_began() {
        // Idle 30s but the state has lasted 50s → there was input 30s ago,
        // i.e. after the state began → "saw it", never fire via AFK.
        assert_eq!(fire_reason(&AFK_ONLY, 50_000, Some(30_000), 0), None);
    }

    #[test]
    fn afk_credits_idle_accrued_before_state_began() {
        // Discussion example: idle X/3 at T_state, X=60s. Fires at 2X/3 in,
        // when total idle first reaches X — not after a full X wait.
        // At 40s in, idle = 20s(before) + 40s = 60s → fires.
        assert_eq!(fire_reason(&AFK_ONLY, 40_000, Some(60_000), 0), Some("afk"));
        // At 39s in, idle = 59s < 60s → not yet.
        assert_eq!(fire_reason(&AFK_ONLY, 39_000, Some(59_000), 0), None);
    }

    #[test]
    fn afk_skipped_when_idle_unknown() {
        assert_eq!(fire_reason(&AFK_ONLY, 999_999, None, 0), None);
    }

    #[test]
    fn reaction_backstop_fires_regardless_of_presence() {
        // User active (idle 0), but the reaction window has elapsed → fire.
        assert_eq!(fire_reason(&BOTH, 120_000, Some(0), 0), Some("reaction"));
        // Before the backstop and not AFK → nothing.
        assert_eq!(fire_reason(&BOTH, 119_999, Some(0), 0), None);
    }

    #[test]
    fn afk_wins_when_both_could_fire() {
        // AFK trips well before the 120s backstop.
        assert_eq!(fire_reason(&BOTH, 65_000, Some(65_000), 0), Some("afk"));
    }

    #[test]
    fn no_windows_set_never_fires() {
        let none = StateNotify { afk_window_ms: None, reaction_window_ms: None };
        assert_eq!(fire_reason(&none, 1_000_000, Some(1_000_000), 0), None);
        // Zero is treated as unset for both windows.
        let zeros = StateNotify { afk_window_ms: Some(0), reaction_window_ms: Some(0) };
        assert_eq!(fire_reason(&zeros, 1_000_000, Some(1_000_000), 0), None);
    }

    // -------- reading-time scaling (reading_time_ms / read_burden_text / fire_reason) --------

    #[test]
    fn reading_time_ms_scales_by_char_count_and_caps() {
        // 300 chars at 15 cps → 20s.
        assert_eq!(reading_time_ms(&"x".repeat(300), 15, READING_CAP_MS), 20_000);
        // Empty / whitespace-only → 0 (behaves as today).
        assert_eq!(reading_time_ms("", 15, READING_CAP_MS), 0);
        assert_eq!(reading_time_ms("   \n\t ", 15, READING_CAP_MS), 0);
        // A wall of text saturates at the cap, never deferring a ping forever.
        assert_eq!(reading_time_ms(&"x".repeat(1_000_000), 15, READING_CAP_MS), READING_CAP_MS);
        // speed 0 disables the scaling regardless of length.
        assert_eq!(reading_time_ms(&"x".repeat(9_000), 0, READING_CAP_MS), 0);
    }

    #[test]
    fn read_burden_text_prefers_last_assistant_then_falls_back_to_label() {
        // blocked/done: the full final assistant turn drives the burden.
        let s = session_with_message("s", Status::Done, 0, "the full multi-paragraph answer");
        assert_eq!(read_burden_text(&s), "the full multi-paragraph answer");
        // No assistant entry (a PreToolUse/permission blocked row) → the label,
        // which there IS the question.
        let mut q = session("s", Status::Blocked, 0);
        q.label = "Can I run bash: pytest?".into();
        assert_eq!(read_burden_text(&q), "Can I run bash: pytest?");
        // A whitespace-only assistant entry also falls back to the label.
        let mut w = session_with_message("s", Status::Blocked, 0, "   ");
        w.label = "has a question".into();
        assert_eq!(read_burden_text(&w), "has a question");
    }

    #[test]
    fn read_burden_text_error_uses_label_not_prior_turn() {
        // A StopFailure Error row still holds the PRIOR successful turn's (long)
        // assistant text, but the actionable content is the short failure kind in
        // `label` — so error reads its label and isn't delayed by the old turn.
        let mut s = session_with_message("s", Status::Error, 0, &"x".repeat(5_000));
        s.label = "api error".into();
        assert_eq!(read_burden_text(&s), "api error");
    }

    #[test]
    fn fire_reason_afk_deferred_by_reading_budget() {
        // 100s reading budget on top of the 60s AFK window → a present silent
        // reader (idle == time_in_state) can't trip AFK until idle >= 160s.
        assert_eq!(fire_reason(&AFK_ONLY, 90_000, Some(90_000), 100_000), None);
        assert_eq!(fire_reason(&AFK_ONLY, 160_000, Some(160_000), 100_000), Some("afk"));
    }

    #[test]
    fn fire_reason_reaction_backstop_deferred_by_reading_budget() {
        // 60s reading budget pushes the 120s backstop out to 180s.
        assert_eq!(fire_reason(&BOTH, 120_000, Some(0), 60_000), None);
        assert_eq!(fire_reason(&BOTH, 180_000, Some(0), 60_000), Some("reaction"));
    }

    #[test]
    fn fire_reason_reading_budget_never_resurrects_a_disabled_window() {
        // AFK-only rule: no reaction window, so no length of message makes the
        // backstop fire — the r > 0 gate runs before the reading_ms addition.
        assert_eq!(fire_reason(&AFK_ONLY, 10_000_000, Some(0), 100_000), None);
    }

    #[tokio::test]
    async fn reconcile_defers_afk_ping_while_reading_long_message() {
        // done, AFK-only 60s. A 1500-char answer at 15 cps adds a 100s reading
        // budget, so the AFK ping holds until idle >= 160s even though the "saw
        // it" guard is satisfied the whole time (present, silent reader).
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let text = "x".repeat(1_500);
        let sessions = vec![session_with_message("s1", Status::Done, 0, &text)];
        let mut out = HashMap::new();
        // 90s in, idle 90s: today this fires (idle > 60s); now suppressed mid-read.
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 90_000, Some(90_000), 15, false).await;
        assert!(out.is_empty(), "present reader of a long message isn't pinged mid-read");
        // 170s in, idle 170s: past the 60s + 100s budget → fires.
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 170_000, Some(170_000), 15, false).await;
        assert_eq!(out.len(), 1, "fires once the reading budget elapses");
    }

    #[tokio::test]
    async fn reconcile_error_ping_not_deferred_by_prior_long_turn() {
        // Error row whose dialog still holds a 5000-char prior turn. Because error
        // reads its short label, the reading budget is ~0 and the actionable
        // backstop (60s) still fires promptly — not delayed toward the cap.
        let m = Mock::with_rules(&[("error", StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: Some(60_000) })]);
        let mut s = session_with_message("s1", Status::Error, 0, &"x".repeat(5_000));
        s.label = "api error".into();
        let sessions = vec![s];
        let mut out = HashMap::new();
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 61_000, Some(0), 15, false).await;
        assert_eq!(out.len(), 1, "error backstop fires off its short label, not the stale long turn");
    }

    #[tokio::test]
    async fn reconcile_fires_done_via_afk_when_user_away() {
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let mut out = HashMap::new();
        // Done for 10s, user idle 70s (away since before it finished).
        let sessions = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 10_000, Some(70_000), 0, false).await;
        assert_eq!(out.len(), 1, "AFK path fires done");
        assert_eq!(out["s1"].for_status, Status::Done);
    }

    #[tokio::test]
    async fn reconcile_suppresses_done_when_user_present() {
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let mut out = HashMap::new();
        // Done for 10s but user touched the machine 2s ago → saw it.
        let sessions = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 10_000, Some(2_000), 0, false).await;
        assert!(out.is_empty(), "present user => no done ping");
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn high_alert_fires_instantly_ignoring_windows_and_reading_budget() {
        // done is AFK-only 60s; a present user (idle 0) with a long unread
        // message would never ping normally. High alert fires it the instant the
        // state is entered, regardless of presence or reading budget.
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let mut out = HashMap::new();
        let sessions = vec![session_with_message("s1", Status::Done, 0, &"x".repeat(5_000))];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 0, Some(0), 15, true).await;
        assert_eq!(out.len(), 1, "high alert pings immediately");
        assert_eq!(out["s1"].for_status, Status::Done);
    }

    #[tokio::test]
    async fn high_alert_does_not_resurrect_a_disabled_state() {
        // A state silenced by zeroing both windows stays silent even under high
        // alert — high alert only accelerates pings that are already configured.
        let m = Mock::with_rules(&[("done", StateNotify { afk_window_ms: Some(0), reaction_window_ms: Some(0) })]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, &mut HashMap::new(), 0, Some(0), 0, true).await;
        assert!(out.is_empty(), "disabled state stays silent under high alert");
        assert!(m.events().is_empty());
    }

    #[test]
    fn rule_notifies_reflects_any_positive_window() {
        assert!(rule_notifies(&StateNotify { afk_window_ms: Some(1), reaction_window_ms: None }));
        assert!(rule_notifies(&StateNotify { afk_window_ms: None, reaction_window_ms: Some(1) }));
        assert!(!rule_notifies(&StateNotify { afk_window_ms: Some(0), reaction_window_ms: Some(0) }));
        assert!(!rule_notifies(&StateNotify { afk_window_ms: None, reaction_window_ms: None }));
    }

    #[test]
    fn status_key_is_exhaustive() {
        assert_eq!(status_key(Status::Idle), "idle");
        assert_eq!(status_key(Status::Working), "working");
        assert_eq!(status_key(Status::Blocked), "blocked");
        assert_eq!(status_key(Status::Done), "done");
        assert_eq!(status_key(Status::Error), "error");
    }

    #[test]
    fn message_text_omits_label_line_when_empty() {
        let s = session("proj", Status::Blocked, 0);
        assert_eq!(build_message_text(&s), "[proj] blocked");
    }

    #[test]
    fn message_text_includes_label_when_present() {
        let mut s = session("proj", Status::Blocked, 0);
        s.label = "Can I run bash: pytest?".into();
        assert_eq!(
            build_message_text(&s),
            "[proj] blocked\nCan I run bash: pytest?"
        );
    }

    #[test]
    fn message_text_uses_custom_display_name_when_set() {
        let mut s = session("proj", Status::Blocked, 0);
        s.display_name = Some("printlab".into());
        assert_eq!(build_message_text(&s), "[printlab] blocked");
    }

    #[test]
    fn message_text_treats_whitespace_only_label_as_empty() {
        let mut s = session("proj", Status::Done, 0);
        s.label = "   ".into();
        assert_eq!(build_message_text(&s), "[proj] done");
    }

    #[test]
    fn message_text_done_shows_task_not_stale_blocked_label() {
        // A row that was Blocked ("needs approval: tool") then settled Done keeps
        // that label (the Stop event carried none), but the done message must
        // show the original task, mirroring the dashboard row — not the stale
        // approval text that made the ping read "done\nneeds approval: tool".
        let mut s = session("printlab", Status::Done, 0);
        s.label = "needs approval: tool".into();
        s.original_prompt = Some("add the print queue page".into());
        assert_eq!(build_message_text(&s), "[printlab] done\nadd the print queue page");
    }

    #[test]
    fn message_text_done_falls_back_to_label_without_original_prompt() {
        // No original_prompt captured → fall back to the label so the line isn't lost.
        let mut s = session("proj", Status::Done, 0);
        s.label = "wrapped up the refactor".into();
        assert_eq!(build_message_text(&s), "[proj] done\nwrapped up the refactor");
    }

    #[test]
    fn message_text_blocked_still_shows_label_over_original_prompt() {
        // For an actionable state the user wants the current question, not the task.
        let mut s = session("proj", Status::Blocked, 0);
        s.label = "Can I run bash: pytest?".into();
        s.original_prompt = Some("add pytest coverage".into());
        assert_eq!(build_message_text(&s), "[proj] blocked\nCan I run bash: pytest?");
    }

    fn ctx_session(id: &str, model: &str, tokens: u64) -> AgentSession {
        let mut s = session(id, Status::Working, 0);
        s.model = Some(model.to_string());
        s.input_tokens = Some(tokens);
        s
    }

    fn windows() -> HashMap<String, u64> {
        [("m".to_string(), 200_000u64)].into_iter().collect()
    }

    #[test]
    fn context_percent_mirrors_frontend_math() {
        let w = windows();
        assert_eq!(context_percent(&ctx_session("s", "m", 100_000), &w), Some(50.0));
        // No tokens, unknown model, or unknown window all yield None.
        assert_eq!(context_percent(&session("s", Status::Working, 0), &w), None);
        assert_eq!(context_percent(&ctx_session("s", "other", 100_000), &w), None);
    }

    #[test]
    fn window_for_matches_longest_prefix() {
        let w: HashMap<String, u64> = [
            ("claude-opus".to_string(), 1_000_000u64),
            ("claude".to_string(), 200_000),
            ("claude-sonnet-4-6".to_string(), 500_000),
        ]
        .into_iter()
        .collect();
        // Family prefixes cover unlisted releases.
        assert_eq!(window_for("claude-opus-4-8", &w), Some(1_000_000));
        assert_eq!(window_for("claude-haiku-4-5", &w), Some(200_000));
        // Exact id beats its family prefix.
        assert_eq!(window_for("claude-sonnet-4-6", &w), Some(500_000));
        // No prefix match at all.
        assert_eq!(window_for("gpt-5", &w), None);
        // Zero-valued entries are skipped, falling through to shorter prefixes.
        let z: HashMap<String, u64> = [("claude-opus".to_string(), 0u64), ("claude".to_string(), 200_000)].into_iter().collect();
        assert_eq!(window_for("claude-opus-4-8", &z), Some(200_000));
    }

    /// Apply a reconcile result to the outstanding map the way the manager loop
    /// does: drop dismissed ids, register a fake handle for each sent session.
    fn apply(outstanding: &mut HashMap<String, String>, to_dismiss: Vec<String>, to_send: Vec<&AgentSession>) {
        for id in to_dismiss {
            outstanding.remove(&id);
        }
        for s in to_send {
            outstanding.insert(s.id.clone(), format!("h-{}", s.id));
        }
    }

    fn sent_ids(to_send: &[&AgentSession]) -> Vec<String> {
        to_send.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn context_alert_fires_once_then_dismisses_on_drop() {
        let w = windows();
        let mut out = HashMap::new();
        // 80% threshold, session at 90% — sends, no dismissals.
        let over = vec![ctx_session("s", "m", 180_000)];
        let (dismiss, send) = context_reconcile(80.0, &over, &w, &out);
        assert_eq!(sent_ids(&send), vec!["s".to_string()]);
        assert!(dismiss.is_empty());
        apply(&mut out, dismiss, send);
        // Still over next tick — no re-send, no dismiss.
        let (dismiss, send) = context_reconcile(80.0, &over, &w, &out);
        assert!(send.is_empty() && dismiss.is_empty());
        // Drops below (new task / clear) — the live alert is dismissed.
        let under = vec![ctx_session("s", "m", 20_000)];
        let (dismiss, send) = context_reconcile(80.0, &under, &w, &out);
        assert_eq!(dismiss, vec!["s".to_string()], "drop below threshold deletes the message");
        assert!(send.is_empty());
        apply(&mut out, dismiss, send);
        assert!(out.is_empty(), "re-armed");
        // Crosses again — sends again.
        let (_, send) = context_reconcile(80.0, &over, &w, &out);
        assert_eq!(sent_ids(&send), vec!["s".to_string()]);
    }

    #[test]
    fn context_alert_fires_at_exact_threshold() {
        let w = windows();
        let out = HashMap::new();
        let s = vec![ctx_session("s", "m", 160_000)]; // exactly 80%
        let (_, send) = context_reconcile(80.0, &s, &w, &out);
        assert_eq!(send.len(), 1);
    }

    #[test]
    fn context_alert_below_threshold_is_silent() {
        let w = windows();
        let out = HashMap::new();
        let s = vec![ctx_session("s", "m", 100_000)]; // 50%
        let (dismiss, send) = context_reconcile(80.0, &s, &w, &out);
        assert!(send.is_empty() && dismiss.is_empty());
    }

    #[test]
    fn context_alert_disabled_dismisses_outstanding() {
        let w = windows();
        let out: HashMap<String, String> = [("s".to_string(), "h-s".to_string())].into_iter().collect();
        let s = vec![ctx_session("s", "m", 180_000)];
        // Threshold 0 disables: the live alert is dismissed even though usage
        // is still high, and nothing new is sent.
        let (dismiss, send) = context_reconcile(0.0, &s, &w, &out);
        assert_eq!(dismiss, vec!["s".to_string()]);
        assert!(send.is_empty());
    }

    #[test]
    fn context_alert_dismisses_when_session_vanishes() {
        let w = windows();
        let mut out = HashMap::new();
        let over = vec![ctx_session("s", "m", 180_000)];
        let (dismiss, send) = context_reconcile(80.0, &over, &w, &out);
        apply(&mut out, dismiss, send);
        // Session gone from snapshot — its alert is dismissed.
        let none: Vec<AgentSession> = vec![];
        let (dismiss, send) = context_reconcile(80.0, &none, &w, &out);
        assert_eq!(dismiss, vec!["s".to_string()]);
        assert!(send.is_empty());
    }

    #[test]
    fn context_alert_ignores_uncomputable_sessions() {
        let w = windows();
        let out = HashMap::new();
        // No model and no tokens — never alerts, never tracked.
        let s = vec![session("s", Status::Working, 0)];
        let (dismiss, send) = context_reconcile(80.0, &s, &w, &out);
        assert!(send.is_empty() && dismiss.is_empty());
    }

    #[test]
    fn context_message_format() {
        let w = windows();
        assert_eq!(
            build_context_message(&ctx_session("proj", "m", 144_000), &w).as_deref(),
            Some("[proj] context 72% (144k/200k)")
        );
        assert_eq!(build_context_message(&session("proj", Status::Working, 0), &w), None);
    }

    #[test]
    fn context_message_uses_custom_display_name_when_set() {
        let w = windows();
        let mut s = ctx_session("proj", "m", 144_000);
        s.display_name = Some("printlab".into());
        assert_eq!(
            build_context_message(&s, &w).as_deref(),
            Some("[printlab] context 72% (144k/200k)")
        );
    }

    // -------- usage-limit reset detection --------
    // The concrete timestamps/percentages below are lifted from real
    // usage_history.jsonl reset events, so these double as regression fixtures.

    const MARGIN: i64 = RESET_JUMP_MARGIN_MS;

    #[test]
    fn window_reset_seeds_silently_then_tracks_peak() {
        let mut w = WindowReset::default();
        // First observation only seeds — never fires, even with a huge later jump
        // being possible; there's no prior window to compare against.
        assert_eq!(w.observe(Some(40.0), Some(1_000_000), 1_000_000, MARGIN), None);
        // Jitter within the margin keeps the same window; peak tracks the max.
        assert_eq!(w.observe(Some(55.0), Some(1_000_030), 1_000_030, MARGIN), None);
        assert_eq!(w.observe(Some(48.0), Some(999_980), 999_980, MARGIN), None);
        assert_eq!(w.peak_pct, 55.0, "peak is the running max, not the latest");
    }

    #[test]
    fn window_reset_fires_on_forward_jump_reporting_peak() {
        // Coarse poll skips the (0, null) transient and lands straight on the
        // jumped resets_at — the forward-jump fallback fires.
        let mut w = WindowReset::default();
        w.observe(Some(90.0), Some(1_000_000), 1_000_000, MARGIN);
        w.observe(Some(96.0), Some(1_000_050), 1_000_050, MARGIN); // peak climbs to 96
        // resets_at jumps a full 5h forward → reset, peak of the ended window.
        let jumped = 1_000_000 + 5 * 3600_000;
        assert_eq!(w.observe(Some(3.0), Some(jumped), jumped, MARGIN), Some(96.0));
        // New window's peak starts fresh at the recovery reading, not carrying 96.
        assert_eq!(w.peak_pct, 3.0);
    }

    #[test]
    fn window_reset_fires_promptly_on_arrived_null_drop() {
        // Real clean 5h reset after a sustained 100%: the reset poll reports
        // (0, null) at the scheduled reset time. The prompt path fires right then,
        // and the later resets_at recovery is consumed silently (no double fire).
        let sched = 1783158000408;
        let mut w = WindowReset::default();
        assert_eq!(w.observe(Some(100.0), Some(sched), sched - 600_000, MARGIN), None);
        assert_eq!(
            w.observe(Some(0.0), None, sched, MARGIN),
            Some(100.0),
            "the scheduled reset has arrived and the percentage is zero → fire now"
        );
        assert_eq!(
            w.observe(Some(2.0), Some(1783176000210), 1783176000210, MARGIN),
            None,
            "the recovery poll is consumed silently — the reset was already reported"
        );
    }

    #[test]
    fn window_reset_fires_when_reset_poll_reports_null_pct() {
        // Some reset polls report (null, null) rather than (0, null). A cleared
        // resets_at at the arrived scheduled time is the reset marker on its own,
        // so the prompt path fires without needing a concrete zero percentage.
        let sched = 1783158000408;
        let mut w = WindowReset::default();
        assert_eq!(w.observe(Some(100.0), Some(sched), sched - 600_000, MARGIN), None);
        assert_eq!(w.observe(None, None, sched, MARGIN), Some(100.0));
    }

    #[test]
    fn window_reset_null_resets_at_with_live_pct_does_not_fire() {
        // Defensive: a cleared resets_at while the percentage is still a live
        // positive value contradicts a reset (a real reset zeroes the percentage),
        // so hold fire and let the forward-jump path settle it if it was real.
        let sched = 1783158000408;
        let mut w = WindowReset::default();
        w.observe(Some(80.0), Some(sched), sched - 600_000, MARGIN);
        assert_eq!(w.observe(Some(80.0), None, sched, MARGIN), None);
    }

    #[test]
    fn window_reset_ignores_collateral_zeroing_via_arrival_gate() {
        // Real early-5h-reset fixture (both buckets zeroed, both resets_at nulled
        // for ~40min): only the 5h window has actually reached its scheduled reset.
        // The 7d window's own reset is still ~4 days out, so the arrival gate
        // rejects its null-drop as collateral and fires 5h only.
        let five_sched = 1782944400185;
        let mut t = ResetTracker::default();
        // poll A: seed both windows, shortly before the 5h reset.
        assert!(t.observe(
            (Some(100.0), Some(five_sched)),
            (Some(51.0), Some(1783306800185)),
            five_sched - 300_000,
            MARGIN,
        ).is_empty());
        // poll B: both zeroed, both resets_at null, at the 5h scheduled time.
        let resets = t.observe((Some(0.0), None), (Some(0.0), None), five_sched, MARGIN);
        assert_eq!(resets, vec![LimitReset { window: LimitWindow::FiveHour, peak_pct: 100.0 }],
            "only the 5h window's reset has arrived; the 7d zeroing is collateral");
        // poll C: 5h resets_at recovers (consumed silently); 7d jitters, no reset.
        let resets = t.observe(
            (Some(10.0), Some(1782960599140)),
            (Some(1.0), Some(1783306799140)),
            1782960599140,
            MARGIN,
        );
        assert!(resets.is_empty(), "recovery is silent; the 7d window never reset");
    }

    #[test]
    fn window_reset_peak_excludes_new_window_reading_on_direct_jump() {
        // A coarse poll interval can skip the transient (0, null) reset poll and
        // land a poll that carries BOTH the jumped resets_at and an
        // already-climbed new-window percentage. That percentage belongs to the
        // new window and must not inflate the just-ended window's peak.
        let mut w = WindowReset::default();
        w.observe(Some(30.0), Some(1_000_000), 1_000_000, MARGIN);
        let jumped = 1_000_000 + 5 * 3600_000;
        assert_eq!(
            w.observe(Some(95.0), Some(jumped), jumped, MARGIN),
            Some(30.0),
            "reports the ended window's 30% peak, not the new window's 95% reading"
        );
        assert_eq!(w.peak_pct, 95.0, "the new window's peak seeds at its first reading");
    }

    #[test]
    fn window_reset_early_rollover_falls_back_to_forward_jump() {
        // Real early reset: 5h rolled over ~90min early; at the (0, null) poll the
        // scheduled time hasn't arrived, so the arrival gate declines the prompt
        // path and the reset is credited later off the +3.5h resets_at jump.
        let sched = 1781047200343;
        let mut w = WindowReset::default();
        w.observe(Some(93.0), Some(sched), sched - 3600_000, MARGIN);
        assert_eq!(
            w.observe(Some(0.0), None, sched - 5_400_000, MARGIN),
            None,
            "90min early: the scheduled reset hasn't arrived → no prompt fire"
        );
        assert_eq!(w.observe(Some(3.0), Some(1781059800867), 1781059800867, MARGIN), Some(93.0));
    }

    #[test]
    fn reset_tracker_reports_both_windows_when_both_reset() {
        let mut t = ResetTracker::default();
        t.observe((Some(95.0), Some(1_000_000)), (Some(92.0), Some(9_000_000)), 1_000_000, MARGIN);
        let five_jump = 1_000_000 + 5 * 3600_000;
        let resets = t.observe(
            (Some(1.0), Some(five_jump)),
            (Some(2.0), Some(9_000_000 + 7 * 86400_000)),
            9_000_000 + 7 * 86400_000,
            MARGIN,
        );
        assert_eq!(resets, vec![
            LimitReset { window: LimitWindow::FiveHour, peak_pct: 95.0 },
            LimitReset { window: LimitWindow::SevenDay, peak_pct: 92.0 },
        ]);
    }

    #[test]
    fn limit_reset_message_format() {
        assert_eq!(
            build_limit_reset_message(LimitReset { window: LimitWindow::FiveHour, peak_pct: 96.4 }),
            "5h limit reset (was 96%)"
        );
        assert_eq!(
            build_limit_reset_message(LimitReset { window: LimitWindow::SevenDay, peak_pct: 91.6 }),
            "7d limit reset (was 92%)"
        );
    }
}
