use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_port: u16,
    pub always_on_top: bool,
    pub save_window_position: bool,
    pub window_position: Option<WindowPosition>,
    pub context_window_tokens: HashMap<String, u64>,
    pub context_bar_thresholds: Vec<Threshold>,
    /// Read by `adapters::claude`: conversational closers that end with '?'
    /// but shouldn't register as awaiting (e.g. "What's next?").
    pub benign_closers: Vec<String>,
    /// Read by `adapters::claude`: used to derive a friendly chat_id from
    /// `cwd`. When a Claude session starts under this directory, the relative
    /// path is used as the session id. None = always use the basename of cwd.
    pub projects_root: Option<String>,
    /// Channel notifications (Telegram today, desktop later). Missing object =
    /// disabled entirely; missing channel inside = that channel disabled.
    pub notifications: Option<NotificationsConfig>,
    /// How often to poll Anthropic's /api/oauth/usage endpoint. Anthropic
    /// rate-limits this endpoint aggressively (see claude-code#31637), so 10
    /// minutes is the conservative default. Clamped to 60s minimum at runtime.
    pub usage_limits_poll_interval_seconds: u64,
    /// Number of segments in the 5h / 7d usage limit bars. Segments scale to
    /// fit the available track width; higher values give finer resolution but
    /// thinner individual segments.
    pub limit_bar_segments: u32,
    /// Auto-resize the window to fit content height. When set to Up, the
    /// bottom edge stays put and the window grows upward; Down keeps the top
    /// edge fixed; None leaves the window manually sized.
    pub auto_resize: AutoResize,
    pub history_font_size: HistoryFontSize,
    pub history_window_position: Option<WindowPosition>,
    /// When the app is auto-launched at login (the "Open to tray" mode), keep
    /// the main window hidden so only the tray icon appears. Read at startup in
    /// `lib.rs`, but only honored when the launch actually came from autostart
    /// (signaled by the `--autostarted` arg) — a manual launch always reveals
    /// the window regardless of this flag. Autostart on/off itself lives in the
    /// OS (registry / LaunchAgent), so this is the only extra bit we persist.
    pub start_minimized: bool,
    /// Read by `state::apply_set`: prompts that suppress the `done`/`idle` →
    /// `working` task boundary. When the user types one of these as a fresh
    /// prompt after the agent has finished, treat it as a continuation of
    /// the previous task rather than a new one — preserve `original_prompt`
    /// and the working timer instead of resetting them. Match is exact,
    /// case-insensitive, after trimming whitespace.
    pub continuation_prompts: Vec<String>,
    /// Mirror each session's status onto its terminal tab title as
    /// "<colored circle> <name>" (e.g. "🔵 ai-dashboard"). Read by
    /// `terminal_title::sync`; Windows-only today.
    pub terminal_titles: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoResize {
    #[default]
    None,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFontSize {
    Smallest,
    Small,
    #[default]
    Regular,
    Large,
    Largest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub telegram: Option<TelegramConfig>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { telegram: Some(TelegramConfig::default()) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
    /// Per-state minimum duration (ms) before firing. Keys must be one of
    /// "idle" | "working" | "awaiting" | "done" | "error". Missing key or
    /// value 0 = silent for that state.
    pub state_thresholds_ms: HashMap<String, u64>,
    /// Context-usage alert: fire a one-shot message when a session's
    /// `input_tokens / context_window_tokens[model]` crosses this percent.
    /// `null` or `0` disables it. Edge-triggered — it fires once on crossing
    /// and re-arms only after usage drops back below the threshold (a new
    /// task or `/clear` resets the token count).
    pub context_alert_percent: Option<f32>,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            state_thresholds_ms: [
                ("awaiting".to_string(), 120_000),
                ("error".to_string(), 60_000),
            ]
            .into_iter()
            .collect(),
            context_alert_percent: Some(80.0),
        }
    }
}

/// Persisted window geometry. `width` / `height` are optional so configs
/// written by older builds (which only stored x/y) keep deserializing.
/// Stored in physical pixels; restoration uses `PhysicalPosition` /
/// `PhysicalSize` so the same monitor reproduces the same window — DPR
/// differences across monitors are an accepted edge case.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WindowPosition {
    pub x: i32,
    pub y: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Threshold {
    pub percent: f32,
    pub color: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_port: 9077,
            always_on_top: true,
            save_window_position: true,
            window_position: None,
            context_window_tokens: [
                ("claude-opus-4-7".to_string(), 1_000_000),
                ("claude-sonnet-4-6".to_string(), 200_000),
                ("claude-haiku-4-5".to_string(), 200_000),
            ]
            .into_iter()
            .collect(),
            context_bar_thresholds: vec![
                Threshold { percent: 0.0, color: "#3a7c4a".into() },
                Threshold { percent: 60.0, color: "#c6a03c".into() },
                Threshold { percent: 85.0, color: "#c64a4a".into() },
            ],
            benign_closers: vec!["What's next?".into(), "Anything else?".into()],
            projects_root: None,
            notifications: Some(NotificationsConfig::default()),
            usage_limits_poll_interval_seconds: 600,
            limit_bar_segments: 16,
            auto_resize: AutoResize::None,
            history_font_size: HistoryFontSize::Regular,
            history_window_position: None,
            start_minimized: false,
            continuation_prompts: vec!["go".into(), "continue".into(), "proceed".into()],
            terminal_titles: true,
        }
    }
}

impl Config {
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("[config] failed to parse {path:?}: {e}; using defaults");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{}".to_string());
        std::fs::write(path, json)
    }
}

pub struct ConfigState {
    pub config: Mutex<Config>,
    pub path: PathBuf,
}

impl ConfigState {
    pub fn new(path: PathBuf) -> Self {
        let config = Config::load_or_default(&path);
        Self {
            config: Mutex::new(config),
            path,
        }
    }

    pub fn snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Config) -> R,
    {
        let mut guard = self.config.lock().unwrap();
        f(&mut guard)
    }

    pub fn save_to_disk(&self) -> std::io::Result<()> {
        let snapshot = self.config.lock().unwrap().clone();
        snapshot.save(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_json_with_only_telegram_creds_backfills_everything_else() {
        // This is the shape of a typical `config/local.json` override —
        // schema evolution must keep this working.
        let partial = r#"{
            "notifications": {
                "telegram": {
                    "bot_token": "t",
                    "chat_id": "c"
                }
            }
        }"#;
        let cfg: Config = serde_json::from_str(partial).expect("partial parse");
        assert_eq!(cfg.server_port, 9077, "default server_port survives");
        assert!(cfg.always_on_top, "default always_on_top survives");
        assert!(
            !cfg.context_window_tokens.is_empty(),
            "default context_window_tokens survives"
        );
        let tg = cfg
            .notifications
            .as_ref()
            .and_then(|n| n.telegram.as_ref())
            .expect("telegram block");
        assert_eq!(tg.bot_token.as_deref(), Some("t"));
        assert_eq!(tg.chat_id.as_deref(), Some("c"));
        assert_eq!(
            tg.state_thresholds_ms.get("awaiting"),
            Some(&120_000),
            "default thresholds survive when caller only supplies creds"
        );
        assert_eq!(tg.state_thresholds_ms.get("error"), Some(&60_000));
        assert_eq!(
            tg.context_alert_percent,
            Some(80.0),
            "default context_alert_percent survives when caller only supplies creds"
        );
    }

    #[test]
    fn context_alert_percent_can_be_overridden_and_disabled() {
        let set: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "context_alert_percent": 70 } } }"#,
        )
        .unwrap();
        let tg = set.notifications.unwrap().telegram.unwrap();
        assert_eq!(tg.context_alert_percent, Some(70.0));

        let off: Config = serde_json::from_str(
            r#"{ "notifications": { "telegram": { "context_alert_percent": null } } }"#,
        )
        .unwrap();
        let tg = off.notifications.unwrap().telegram.unwrap();
        assert_eq!(tg.context_alert_percent, None);
    }

    #[test]
    fn empty_json_object_gives_full_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        let tg = cfg.notifications.unwrap().telegram.unwrap();
        assert!(tg.bot_token.is_none());
        assert_eq!(tg.state_thresholds_ms.get("awaiting"), Some(&120_000));
    }

    #[test]
    fn auto_resize_defaults_to_none_when_field_missing() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.auto_resize, AutoResize::None);
    }

    #[test]
    fn auto_resize_parses_snake_case() {
        let up: Config = serde_json::from_str(r#"{ "auto_resize": "up" }"#).unwrap();
        assert_eq!(up.auto_resize, AutoResize::Up);
        let down: Config = serde_json::from_str(r#"{ "auto_resize": "down" }"#).unwrap();
        assert_eq!(down.auto_resize, AutoResize::Down);
    }

    #[test]
    fn unknown_fields_are_silently_ignored_so_renames_are_survivable() {
        let with_extra = r#"{ "this_key_does_not_exist_on_config": 42 }"#;
        let cfg: Config = serde_json::from_str(with_extra).unwrap();
        assert_eq!(cfg.server_port, 9077);
    }

    #[test]
    fn terminal_titles_defaults_on_when_field_missing() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.terminal_titles);
        let off: Config = serde_json::from_str(r#"{ "terminal_titles": false }"#).unwrap();
        assert!(!off.terminal_titles);
    }

    #[test]
    fn continuation_prompts_default_includes_common_phrases() {
        let cfg = Config::default();
        assert!(cfg.continuation_prompts.iter().any(|s| s == "go"));
        assert!(cfg.continuation_prompts.iter().any(|s| s == "continue"));
        assert!(cfg.continuation_prompts.iter().any(|s| s == "proceed"));
    }

    #[test]
    fn continuation_prompts_can_be_overridden_by_partial_json() {
        let partial = r#"{ "continuation_prompts": ["yes", "go ahead"] }"#;
        let cfg: Config = serde_json::from_str(partial).unwrap();
        assert_eq!(cfg.continuation_prompts, vec!["yes".to_string(), "go ahead".to_string()]);
        // unrelated defaults still survive
        assert_eq!(cfg.server_port, 9077);
    }
}
