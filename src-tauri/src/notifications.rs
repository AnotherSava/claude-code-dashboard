use async_trait::async_trait;
use std::collections::HashMap;
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
}
