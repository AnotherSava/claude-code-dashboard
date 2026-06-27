use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use crate::config::{StateNotify, TelegramConfig};
use crate::notifications::{build_message_text, Notifier};
use crate::state::AgentSession;

#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Unchanged,
    CredsChanged,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Creds {
    bot_token: String,
    chat_id: String,
}

#[derive(Default)]
struct Inner {
    creds: Option<Creds>,
    rules: HashMap<String, StateNotify>,
}

pub struct TelegramNotifier {
    client: reqwest::Client,
    inner: RwLock<Inner>,
}

impl TelegramNotifier {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Reconcile internal state against current config. Returns whether the
    /// effective credentials changed, so the caller can decide whether to
    /// drop the outstanding-message map (messages sent under the old bot
    /// can't be deleted with the new credentials).
    pub fn sync_config(&self, cfg: Option<&TelegramConfig>) -> SyncOutcome {
        let new_creds = cfg.and_then(|c| {
            let token = c
                .bot_token
                .as_ref()
                .filter(|t| !t.trim().is_empty())?
                .clone();
            let chat = c
                .chat_id
                .as_ref()
                .filter(|t| !t.trim().is_empty())?
                .clone();
            Some(Creds { bot_token: token, chat_id: chat })
        });
        let new_rules = cfg.map(|c| c.states.clone()).unwrap_or_default();

        let mut inner = self.inner.write().unwrap();
        let prev = inner.creds.clone();
        inner.rules = new_rules;
        inner.creds = new_creds.clone();

        match (prev, new_creds) {
            (None, None) => SyncOutcome::Disabled,
            (Some(a), Some(b)) if a == b => SyncOutcome::Unchanged,
            (Some(_), None) => SyncOutcome::Disabled,
            _ => SyncOutcome::CredsChanged,
        }
    }

    fn creds(&self) -> Option<Creds> {
        self.inner.read().unwrap().creds.clone()
    }

    async fn call(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, TelegramCallError> {
        let creds = self.creds().ok_or(TelegramCallError::Disabled)?;
        let url = format!("https://api.telegram.org/bot{}/{}", creds.bot_token, method);
        let resp = self.client.post(&url).json(&body).send().await?;
        let http_status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        let ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if http_status.is_success() && ok {
            Ok(body)
        } else {
            let code = body
                .get("error_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(http_status.as_u16() as i64);
            let desc = body
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Err(TelegramCallError::Api { code, description: desc })
        }
    }

    /// Send a free-form message — used by the `test_telegram_notification`
    /// command. Returns the message_id stringified.
    pub async fn send_raw(&self, text: &str) -> anyhow::Result<String> {
        let creds = self.creds().ok_or_else(|| anyhow::anyhow!("telegram disabled"))?;
        let body = serde_json::json!({ "chat_id": creds.chat_id, "text": text });
        let resp = self.call("sendMessage", body).await?;
        let id = resp
            .pointer("/result/message_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("missing result.message_id in sendMessage response"))?;
        Ok(id.to_string())
    }
}

#[derive(Debug)]
enum TelegramCallError {
    Disabled,
    Http(reqwest::Error),
    Api { code: i64, description: String },
}

impl std::fmt::Display for TelegramCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "telegram disabled (no credentials)"),
            Self::Http(e) => write!(f, "http error: {e}"),
            Self::Api { code, description } => {
                write!(f, "telegram api error {code}: {description}")
            }
        }
    }
}

impl std::error::Error for TelegramCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for TelegramCallError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    fn channel_name(&self) -> &'static str {
        "telegram"
    }

    fn is_enabled(&self) -> bool {
        self.inner.read().unwrap().creds.is_some()
    }

    fn state_rules(&self) -> HashMap<String, StateNotify> {
        self.inner.read().unwrap().rules.clone()
    }

    async fn send(&self, session: &AgentSession) -> anyhow::Result<String> {
        let text = build_message_text(session);
        self.send_raw(&text).await
    }

    async fn dismiss(&self, handle: &str) -> anyhow::Result<()> {
        let creds = self
            .creds()
            .ok_or_else(|| anyhow::anyhow!("telegram disabled"))?;
        let message_id: i64 = handle
            .parse()
            .map_err(|e| anyhow::anyhow!("bad telegram handle {}: {}", handle, e))?;
        let body = serde_json::json!({ "chat_id": creds.chat_id, "message_id": message_id });
        match self.call("deleteMessage", body).await {
            Ok(_) => Ok(()),
            Err(TelegramCallError::Api { code, description }) if code == 400 || code == 403 => {
                // 400 "message to delete not found" / ">48h old", 403 "bot blocked" —
                // nothing to do, the message is effectively gone either way.
                tracing::debug!(code, %description, handle, "telegram dismiss benign");
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(token: Option<&str>, chat: Option<&str>, thresholds: &[(&str, u64)]) -> TelegramConfig {
        TelegramConfig {
            bot_token: token.map(String::from),
            chat_id: chat.map(String::from),
            states: thresholds
                .iter()
                .map(|(k, v)| (k.to_string(), StateNotify { afk_window_ms: None, reaction_window_ms: Some(*v) }))
                .collect(),
            context_alert_percent: None,
        }
    }

    #[test]
    fn sync_from_empty_to_empty_is_disabled() {
        let n = TelegramNotifier::new();
        assert_eq!(n.sync_config(None), SyncOutcome::Disabled);
        assert!(!n.is_enabled());
    }

    #[test]
    fn sync_missing_token_is_disabled() {
        let n = TelegramNotifier::new();
        let c = cfg(None, Some("123"), &[]);
        assert_eq!(n.sync_config(Some(&c)), SyncOutcome::Disabled);
        assert!(!n.is_enabled());
    }

    #[test]
    fn sync_empty_string_token_is_disabled() {
        let n = TelegramNotifier::new();
        let c = cfg(Some("   "), Some("123"), &[]);
        assert_eq!(n.sync_config(Some(&c)), SyncOutcome::Disabled);
        assert!(!n.is_enabled());
    }

    #[test]
    fn sync_sets_credentials_and_rules() {
        let n = TelegramNotifier::new();
        let c = cfg(Some("t"), Some("c"), &[("blocked", 60_000)]);
        assert_eq!(n.sync_config(Some(&c)), SyncOutcome::CredsChanged);
        assert!(n.is_enabled());
        assert_eq!(n.state_rules().get("blocked").and_then(|s| s.reaction_window_ms), Some(60_000));
    }

    #[test]
    fn sync_unchanged_when_same_creds() {
        let n = TelegramNotifier::new();
        let c = cfg(Some("t"), Some("c"), &[("blocked", 60_000)]);
        let _ = n.sync_config(Some(&c));
        // rule change alone is not a credential change
        let c2 = cfg(Some("t"), Some("c"), &[("blocked", 120_000)]);
        assert_eq!(n.sync_config(Some(&c2)), SyncOutcome::Unchanged);
        assert_eq!(n.state_rules().get("blocked").and_then(|s| s.reaction_window_ms), Some(120_000));
    }

    #[test]
    fn sync_detects_token_change() {
        let n = TelegramNotifier::new();
        let _ = n.sync_config(Some(&cfg(Some("t1"), Some("c"), &[])));
        assert_eq!(
            n.sync_config(Some(&cfg(Some("t2"), Some("c"), &[]))),
            SyncOutcome::CredsChanged
        );
    }

    #[test]
    fn sync_detects_chat_change() {
        let n = TelegramNotifier::new();
        let _ = n.sync_config(Some(&cfg(Some("t"), Some("c1"), &[])));
        assert_eq!(
            n.sync_config(Some(&cfg(Some("t"), Some("c2"), &[]))),
            SyncOutcome::CredsChanged
        );
    }

    #[test]
    fn sync_clearing_creds_reports_disabled() {
        let n = TelegramNotifier::new();
        let _ = n.sync_config(Some(&cfg(Some("t"), Some("c"), &[])));
        assert_eq!(n.sync_config(None), SyncOutcome::Disabled);
        assert!(!n.is_enabled());
    }
}
