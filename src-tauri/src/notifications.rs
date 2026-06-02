use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::config::ConfigState;
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
    fn thresholds(&self) -> HashMap<String, u64>;
    async fn send(&self, session: &AgentSession) -> anyhow::Result<String>;
    async fn dismiss(&self, handle: &str) -> anyhow::Result<()>;
}

pub fn status_key(s: Status) -> &'static str {
    match s {
        Status::Idle => "idle",
        Status::Working => "working",
        Status::Awaiting => "awaiting",
        Status::Done => "done",
        Status::Error => "error",
    }
}

pub fn build_message_text(session: &AgentSession) -> String {
    let status = status_key(session.status);
    if session.label.trim().is_empty() {
        format!("[{}] {}", session.id, status)
    } else {
        format!("[{}] {}\n{}", session.id, status, session.label)
    }
}

/// A session's context usage as a percent of its model's window, mirroring
/// the frontend's `tokenColor` math. `None` when the session has no token
/// count, no model, or no configured window for that model.
pub fn context_percent(session: &AgentSession, window_tokens: &HashMap<String, u64>) -> Option<f32> {
    let tokens = session.input_tokens?;
    let max = window_tokens.get(session.model.as_ref()?).copied().filter(|m| *m > 0)?;
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
    let max = window_tokens.get(session.model.as_ref()?).copied()?;
    Some(format!("[{}] context {}% ({}/{})", session.id, pct.round() as u32, tokens_k(tokens), tokens_k(max)))
}

/// Edge-triggered selection of sessions that newly crossed `threshold_percent`
/// of context this tick. `fired` holds the ids already alerted and still above
/// the threshold; it is rewritten each call to the currently-over set, so a
/// session re-arms once its usage drops below (or its window/model becomes
/// unknown, or it vanishes). A `threshold_percent <= 0` disables the feature
/// and clears `fired`.
pub fn context_alerts_due<'a>(
    threshold_percent: f32,
    sessions: &'a [AgentSession],
    window_tokens: &HashMap<String, u64>,
    fired: &mut HashSet<String>,
) -> Vec<&'a AgentSession> {
    if threshold_percent <= 0.0 {
        fired.clear();
        return Vec::new();
    }
    let is_over = |s: &AgentSession| context_percent(s, window_tokens).is_some_and(|p| p >= threshold_percent);
    let due: Vec<&AgentSession> = sessions.iter().filter(|s| is_over(s) && !fired.contains(&s.id)).collect();
    *fired = sessions.iter().filter(|s| is_over(s)).map(|s| s.id.clone()).collect();
    due
}

pub async fn reconcile(
    notifier: &dyn Notifier,
    sessions: &[AgentSession],
    outstanding: &mut HashMap<String, Outstanding>,
    now_ms: i64,
) {
    let thresholds = notifier.thresholds();

    let stale: Vec<(String, Outstanding)> = outstanding
        .iter()
        .filter(|(id, o)| {
            sessions
                .iter()
                .find(|s| &s.id == *id)
                .map_or(true, |s| s.status != o.for_status)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (id, o) in stale {
        if let Err(e) = notifier.dismiss(&o.handle).await {
            tracing::debug!(
                channel = notifier.channel_name(),
                handle = %o.handle,
                ?e,
                "dismiss failed"
            );
        }
        outstanding.remove(&id);
    }

    for s in sessions {
        if outstanding.contains_key(&s.id) {
            continue;
        }
        let key = status_key(s.status);
        let Some(&threshold) = thresholds.get(key) else { continue };
        if threshold == 0 {
            continue;
        }
        if (now_ms - s.state_entered_at) < threshold as i64 {
            continue;
        }
        match notifier.send(s).await {
            Ok(handle) => {
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
            let mut context_fired: HashSet<String> = HashSet::new();
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
                let sessions = app_state.snapshot();

                let tg_cfg = cfg
                    .notifications
                    .as_ref()
                    .and_then(|n| n.telegram.as_ref());

                let outcome = telegram.sync_config(tg_cfg);
                if matches!(outcome, SyncOutcome::CredsChanged | SyncOutcome::Disabled)
                    && !outstanding.is_empty()
                {
                    tracing::warn!(
                        channel = "telegram",
                        reason = ?outcome,
                        count = outstanding.len(),
                        "credentials changed or disabled; dropping outstanding map without deleting"
                    );
                    outstanding.clear();
                }

                if telegram.is_enabled() {
                    reconcile(
                        telegram.as_ref() as &dyn Notifier,
                        &sessions,
                        &mut outstanding,
                        now_ms(),
                    )
                    .await;

                    let threshold = tg_cfg.and_then(|c| c.context_alert_percent).unwrap_or(0.0);
                    let due = context_alerts_due(threshold, &sessions, &cfg.context_window_tokens, &mut context_fired);
                    for s in due {
                        let Some(text) = build_context_message(s, &cfg.context_window_tokens) else { continue };
                        if let Err(e) = telegram.send_raw(&text).await {
                            tracing::warn!(channel = "telegram", id = %s.id, ?e, "context alert send failed");
                            // Re-arm so the next tick retries this session.
                            context_fired.remove(&s.id);
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
        thresholds: HashMap<String, u64>,
        events: Mutex<Vec<Event>>,
        handle_counter: Mutex<u64>,
        send_err: Mutex<bool>,
    }

    impl Mock {
        fn with(thresholds: &[(&str, u64)]) -> Arc<Self> {
            Arc::new(Self {
                thresholds: thresholds.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
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
        fn thresholds(&self) -> HashMap<String, u64> { self.thresholds.clone() }
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
        }
    }

    #[tokio::test]
    async fn sends_when_threshold_elapsed_and_no_outstanding() {
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 60_000).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out["s1"].for_status, Status::Awaiting);
        assert_eq!(out["s1"].handle, "h1");
        assert!(matches!(m.events()[0], Event::Send { .. }));
    }

    #[tokio::test]
    async fn does_not_send_before_threshold() {
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 59_999).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn noop_when_outstanding_matches_current_state() {
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h1".into(), for_status: Status::Awaiting, },
        );
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 120_000).await;
        assert_eq!(out.len(), 1);
        assert!(m.events().is_empty(), "no events when nothing changes");
    }

    #[tokio::test]
    async fn dismisses_when_session_transitions_to_different_state() {
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h9".into(), for_status: Status::Awaiting, },
        );
        let sessions = vec![session("s1", Status::Working, 100_000)];
        reconcile(m.as_ref(), &sessions, &mut out, 120_000).await;
        assert!(out.is_empty());
        assert_eq!(m.events(), vec![Event::Dismiss { handle: "h9".into() }]);
    }

    #[tokio::test]
    async fn dismisses_when_session_vanishes_from_snapshot() {
        // This is the "user clicked × on the widget row" path — session
        // disappears entirely from AppState::snapshot().
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        out.insert(
            "s1".to_string(),
            Outstanding { handle: "h7".into(), for_status: Status::Awaiting, },
        );
        let sessions: Vec<AgentSession> = vec![];
        reconcile(m.as_ref(), &sessions, &mut out, 120_000).await;
        assert!(out.is_empty());
        assert_eq!(m.events(), vec![Event::Dismiss { handle: "h7".into() }]);
    }

    #[tokio::test]
    async fn session_vanishes_mid_threshold_is_noop() {
        // User clicks × 30s into a 60s threshold; no outstanding exists yet.
        let m = Mock::with(&[("awaiting", 60_000)]);
        let mut out = HashMap::new();
        let sessions: Vec<AgentSession> = vec![];
        reconcile(m.as_ref(), &sessions, &mut out, 30_000).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn threshold_zero_means_silent() {
        let m = Mock::with(&[("awaiting", 0)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 1_000_000).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn missing_threshold_key_means_silent() {
        let m = Mock::with(&[("error", 60_000)]);
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 1_000_000).await;
        assert!(out.is_empty());
        assert!(m.events().is_empty());
    }

    #[tokio::test]
    async fn send_failure_leaves_no_outstanding_so_next_tick_retries() {
        let m = Mock::with(&[("awaiting", 60_000)]);
        *m.send_err.lock().unwrap() = true;
        let mut out = HashMap::new();
        let sessions = vec![session("s1", Status::Awaiting, 0)];
        reconcile(m.as_ref(), &sessions, &mut out, 60_000).await;
        assert!(out.is_empty(), "failed send must not populate outstanding");
    }

    #[test]
    fn status_key_is_exhaustive() {
        assert_eq!(status_key(Status::Idle), "idle");
        assert_eq!(status_key(Status::Working), "working");
        assert_eq!(status_key(Status::Awaiting), "awaiting");
        assert_eq!(status_key(Status::Done), "done");
        assert_eq!(status_key(Status::Error), "error");
    }

    #[test]
    fn message_text_omits_label_line_when_empty() {
        let s = session("proj", Status::Awaiting, 0);
        assert_eq!(build_message_text(&s), "[proj] awaiting");
    }

    #[test]
    fn message_text_includes_label_when_present() {
        let mut s = session("proj", Status::Awaiting, 0);
        s.label = "Can I run bash: pytest?".into();
        assert_eq!(
            build_message_text(&s),
            "[proj] awaiting\nCan I run bash: pytest?"
        );
    }

    #[test]
    fn message_text_treats_whitespace_only_label_as_empty() {
        let mut s = session("proj", Status::Done, 0);
        s.label = "   ".into();
        assert_eq!(build_message_text(&s), "[proj] done");
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
    fn context_alert_fires_once_then_dedups_until_drop() {
        let w = windows();
        let mut fired = HashSet::new();
        // 80% threshold, session at 90% — fires.
        let over = vec![ctx_session("s", "m", 180_000)];
        let due = context_alerts_due(80.0, &over, &w, &mut fired);
        assert_eq!(due.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["s"]);
        // Still over next tick — no re-fire.
        let due = context_alerts_due(80.0, &over, &w, &mut fired);
        assert!(due.is_empty());
        // Drops below (new task / clear) — re-arms.
        let under = vec![ctx_session("s", "m", 20_000)];
        let due = context_alerts_due(80.0, &under, &w, &mut fired);
        assert!(due.is_empty());
        assert!(fired.is_empty(), "re-armed");
        // Crosses again — fires again.
        let due = context_alerts_due(80.0, &over, &w, &mut fired);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn context_alert_fires_at_exact_threshold() {
        let w = windows();
        let mut fired = HashSet::new();
        let s = vec![ctx_session("s", "m", 160_000)]; // exactly 80%
        assert_eq!(context_alerts_due(80.0, &s, &w, &mut fired).len(), 1);
    }

    #[test]
    fn context_alert_below_threshold_is_silent() {
        let w = windows();
        let mut fired = HashSet::new();
        let s = vec![ctx_session("s", "m", 100_000)]; // 50%
        assert!(context_alerts_due(80.0, &s, &w, &mut fired).is_empty());
        assert!(fired.is_empty());
    }

    #[test]
    fn context_alert_disabled_clears_fired() {
        let w = windows();
        let mut fired: HashSet<String> = ["s".to_string()].into_iter().collect();
        let s = vec![ctx_session("s", "m", 180_000)];
        assert!(context_alerts_due(0.0, &s, &w, &mut fired).is_empty());
        assert!(fired.is_empty(), "disabled threshold clears tracking");
    }

    #[test]
    fn context_alert_rearms_when_session_vanishes() {
        let w = windows();
        let mut fired = HashSet::new();
        let over = vec![ctx_session("s", "m", 180_000)];
        assert_eq!(context_alerts_due(80.0, &over, &w, &mut fired).len(), 1);
        // Session gone from snapshot — fired must not retain it.
        let none: Vec<AgentSession> = vec![];
        assert!(context_alerts_due(80.0, &none, &w, &mut fired).is_empty());
        assert!(fired.is_empty());
    }

    #[test]
    fn context_alert_ignores_uncomputable_sessions() {
        let w = windows();
        let mut fired = HashSet::new();
        // No model and no tokens — never alerts, never tracked.
        let s = vec![session("s", Status::Working, 0)];
        assert!(context_alerts_due(80.0, &s, &w, &mut fired).is_empty());
        assert!(fired.is_empty());
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
}
