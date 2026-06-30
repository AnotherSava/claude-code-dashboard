use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::config::{ConfigState, StateNotify};
use crate::custom_names::CustomNamesStore;
use crate::state::{AgentSession, AppState, Status};
use crate::telegram::{SyncOutcome, TelegramNotifier};

#[derive(Clone, Debug)]
pub struct Outstanding {
    pub handle: String,
    pub for_status: Status,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn channel_name(&self) -> &'static str;
    fn is_enabled(&self) -> bool;
    fn state_rules(&self) -> HashMap<String, StateNotify>;
    async fn send(&self, session: &AgentSession) -> anyhow::Result<String>;
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

/// Decide whether a state in `rule` is due to fire, given how long it has been
/// in that state and the system-wide input-idle time. Returns the reason it
/// fired (for logging), or `None` if neither criterion is met.
///
/// - **AFK** (`afk_window_ms`): the user is idle past the window *and* has not
///   touched the machine since the state began (`idle >= time_in_state`, the
///   "saw it" guard). Skipped when idle is unknown (presence can't be proven).
/// - **Reaction** (`reaction_window_ms`): the state has outlasted the backstop
///   regardless of presence.
///
/// Whichever trips first wins; AFK lets a notification fire sooner once the
/// user has stepped away.
pub fn fire_reason(rule: &StateNotify, time_in_state_ms: u64, idle_ms: Option<u64>) -> Option<&'static str> {
    let afk_due = matches!((rule.afk_window_ms, idle_ms), (Some(afk), Some(idle)) if afk > 0 && idle >= afk && idle >= time_in_state_ms);
    if afk_due {
        return Some("afk");
    }
    let reaction_due = matches!(rule.reaction_window_ms, Some(r) if r > 0 && time_in_state_ms >= r);
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

pub async fn reconcile(
    notifier: &dyn Notifier,
    sessions: &[AgentSession],
    outstanding: &mut HashMap<String, Outstanding>,
    now_ms: i64,
    idle_ms: Option<u64>,
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

    for s in sessions {
        if outstanding.contains_key(&s.id) {
            continue;
        }
        let key = status_key(s.status);
        let Some(rule) = rules.get(key) else { continue };
        let time_in_state = (now_ms - s.state_entered_at).max(0) as u64;
        let Some(reason) = fire_reason(rule, time_in_state, idle_ms) else { continue };
        match notifier.send(s).await {
            Ok(handle) => {
                tracing::debug!(
                    channel = notifier.channel_name(),
                    id = %s.id,
                    status = key,
                    reason,
                    "notification fired"
                );
                outstanding.insert(
                    s.id.clone(),
                    Outstanding { handle, for_status: s.status },
                );
            }
            Err(e) => {
                tracing::warn!(
                    channel = notifier.channel_name(),
                    id = %s.id,
                    ?e,
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
            // Live context-usage alerts: session id -> Telegram message handle,
            // so the message can be deleted once usage drops back below.
            let mut context_outstanding: HashMap<String, String> = HashMap::new();
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
                if matches!(outcome, SyncOutcome::CredsChanged | SyncOutcome::Disabled)
                    && (!outstanding.is_empty() || !context_outstanding.is_empty())
                {
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

                if telegram.is_enabled() {
                    reconcile(
                        telegram.as_ref() as &dyn Notifier,
                        &sessions,
                        &mut outstanding,
                        now_ms(),
                        crate::idle::idle_ms(),
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
    use crate::state::{AgentSession, Status};
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
        send_err: Mutex<bool>,
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
                send_err: Mutex::new(false),
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
        async fn send(&self, session: &AgentSession) -> anyhow::Result<String> {
            if *self.send_err.lock().unwrap() {
                return Err(anyhow::anyhow!("boom"));
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

    #[tokio::test]
    async fn sends_when_threshold_elapsed_and_no_outstanding() {
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 60_000, None).await;
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
        reconcile(m.as_ref(), &sessions, &mut out, 59_999, None).await;
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
        reconcile(m.as_ref(), &sessions, &mut out, 120_000, None).await;
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
        reconcile(m.as_ref(), &sessions, &mut out, 120_000, None).await;
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
        reconcile(m.as_ref(), &sessions, &mut out, 120_000, None).await;
        assert!(out.is_empty());
        assert_eq!(m.events(), vec![Event::Dismiss { handle: "h7".into() }]);
    }

    #[tokio::test]
    async fn session_vanishes_mid_threshold_is_noop() {
        // User clicks × 30s into a 60s threshold; no outstanding exists yet.
        let m = Mock::with(&[("blocked", 60_000)]);
        let mut out = HashMap::new();
        let sessions: Vec<AgentSession> = vec![];
        reconcile(m.as_ref(), &sessions, &mut out, 30_000, None).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn threshold_zero_means_silent() {
        let m = Mock::with(&[("blocked", 0)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 1_000_000, None).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn missing_threshold_key_means_silent() {
        let m = Mock::with(&[("error", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 1_000_000, None).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn send_failure_leaves_no_outstanding_so_next_tick_retries() {
        let m = Mock::with(&[("blocked", 60_000)]);
        *m.send_err.lock().unwrap() = true;
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Blocked, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 60_000, None).await;
        assert!(out.is_empty(), "failed send must not populate outstanding");
    }

    // -------- fire_reason (per-state AFK + reaction rule) --------

    const AFK_ONLY: StateNotify = StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: None };
    const BOTH: StateNotify = StateNotify { afk_window_ms: Some(60_000), reaction_window_ms: Some(120_000) };

    #[test]
    fn afk_fires_when_idle_past_window_and_no_input_since_state_began() {
        // 10s into the state, user idle 70s — away the whole time, fires.
        assert_eq!(fire_reason(&AFK_ONLY, 10_000, Some(70_000)), Some("afk"));
    }

    #[test]
    fn afk_suppressed_when_user_active_since_state_began() {
        // Idle 30s but the state has lasted 50s → there was input 30s ago,
        // i.e. after the state began → "saw it", never fire via AFK.
        assert_eq!(fire_reason(&AFK_ONLY, 50_000, Some(30_000)), None);
    }

    #[test]
    fn afk_credits_idle_accrued_before_state_began() {
        // Discussion example: idle X/3 at T_state, X=60s. Fires at 2X/3 in,
        // when total idle first reaches X — not after a full X wait.
        // At 40s in, idle = 20s(before) + 40s = 60s → fires.
        assert_eq!(fire_reason(&AFK_ONLY, 40_000, Some(60_000)), Some("afk"));
        // At 39s in, idle = 59s < 60s → not yet.
        assert_eq!(fire_reason(&AFK_ONLY, 39_000, Some(59_000)), None);
    }

    #[test]
    fn afk_skipped_when_idle_unknown() {
        assert_eq!(fire_reason(&AFK_ONLY, 999_999, None), None);
    }

    #[test]
    fn reaction_backstop_fires_regardless_of_presence() {
        // User active (idle 0), but the reaction window has elapsed → fire.
        assert_eq!(fire_reason(&BOTH, 120_000, Some(0)), Some("reaction"));
        // Before the backstop and not AFK → nothing.
        assert_eq!(fire_reason(&BOTH, 119_999, Some(0)), None);
    }

    #[test]
    fn afk_wins_when_both_could_fire() {
        // AFK trips well before the 120s backstop.
        assert_eq!(fire_reason(&BOTH, 65_000, Some(65_000)), Some("afk"));
    }

    #[test]
    fn no_windows_set_never_fires() {
        let none = StateNotify { afk_window_ms: None, reaction_window_ms: None };
        assert_eq!(fire_reason(&none, 1_000_000, Some(1_000_000)), None);
        // Zero is treated as unset for both windows.
        let zeros = StateNotify { afk_window_ms: Some(0), reaction_window_ms: Some(0) };
        assert_eq!(fire_reason(&zeros, 1_000_000, Some(1_000_000)), None);
    }

    #[tokio::test]
    async fn reconcile_fires_done_via_afk_when_user_away() {
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let mut out = HashMap::new();
        // Done for 10s, user idle 70s (away since before it finished).
        let sessions = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 10_000, Some(70_000)).await;
        assert_eq!(out.len(), 1, "AFK path fires done");
        assert_eq!(out["s1"].for_status, Status::Done);
    }

    #[tokio::test]
    async fn reconcile_suppresses_done_when_user_present() {
        let m = Mock::with_rules(&[("done", AFK_ONLY)]);
        let mut out = HashMap::new();
        // Done for 10s but user touched the machine 2s ago → saw it.
        let sessions = vec![session("s1", Status::Done, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 10_000, Some(2_000)).await;
        assert!(out.is_empty(), "present user => no done ping");
        assert!(m.events().is_empty());
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
}
