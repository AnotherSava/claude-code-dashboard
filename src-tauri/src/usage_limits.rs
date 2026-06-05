use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;

use crate::commands::{emit_usage_limits_updated, now_ms};
use crate::config::ConfigState;
use crate::usage_history::{UsageHistoryRecord, UsageHistoryStore};

#[derive(Clone, Debug, Serialize)]
pub struct LimitBucket {
    pub utilization: f32,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    Unavailable,
    AuthExpired,
    NetworkError,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageLimits {
    pub five_hour: Option<LimitBucket>,
    pub seven_day: Option<LimitBucket>,
    pub status: UsageStatus,
    pub updated: i64,
}

impl UsageLimits {
    fn empty() -> Self {
        Self {
            five_hour: None,
            seven_day: None,
            status: UsageStatus::Unavailable,
            updated: 0,
        }
    }
}

pub struct UsageLimitsState {
    inner: RwLock<UsageLimits>,
    wake: Arc<Notify>,
    /// Last time we hit the OAuth refresh endpoint. Used to gate refresh
    /// attempts to one per `REFRESH_COOLDOWN_SECS` so a sequence of failed
    /// refreshes doesn't hammer Anthropic.
    last_refresh_attempt: Mutex<Option<Instant>>,
    /// Set the first time a poll observes an expired access token; cleared
    /// when a fresh token is observed. The first expired poll only sets
    /// this and reports auth_expired — refresh is deferred to the next
    /// poll cycle. Gives Claude Code a chance to refresh on its own first,
    /// avoiding a race over the rotating refresh_token.
    saw_expired_last_poll: AtomicBool,
}

impl UsageLimitsState {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(UsageLimits::empty()),
            wake: Arc::new(Notify::new()),
            last_refresh_attempt: Mutex::new(None),
            saw_expired_last_poll: AtomicBool::new(false),
        }
    }

    pub fn snapshot(&self) -> UsageLimits {
        self.inner.read().unwrap().clone()
    }

    fn replace(&self, next: UsageLimits) {
        *self.inner.write().unwrap() = next;
    }

    /// Ask the poller to run a fresh poll ASAP. Rate-limited: if the last
    /// poll attempt landed within `MIN_POLL_SECS` we drop the request and
    /// return `false`, so spam from the frontend can't exceed the Anthropic
    /// rate-limit floor.
    pub fn request_refresh(&self) -> bool {
        let updated = self.snapshot().updated;
        // Special-case the pre-first-poll state: `updated == 0` means we've
        // never written a snapshot yet, so `now_ms() - 0` would log a
        // wall-clock-since-epoch value that looks like a bug.
        if updated == 0 {
            tracing::debug!("refresh request granted, waking poller (no prior poll)");
            self.wake.notify_one();
            return true;
        }
        let age_ms = now_ms() - updated;
        if age_ms < (MIN_POLL_SECS * 1000) as i64 {
            tracing::debug!(age_ms, "refresh request denied (inside floor)");
            return false;
        }
        tracing::debug!(age_ms, "refresh request granted, waking poller");
        self.wake.notify_one();
        true
    }

    /// Reserve a refresh-attempt slot if the cooldown has elapsed. Returns
    /// `true` if the caller should proceed, `false` if we should wait.
    fn claim_refresh_slot(&self) -> bool {
        let mut guard = self.last_refresh_attempt.lock().unwrap();
        let allowed = guard.map_or(true, |t| {
            t.elapsed() >= Duration::from_secs(REFRESH_COOLDOWN_SECS)
        });
        if allowed {
            *guard = Some(Instant::now());
        }
        allowed
    }
}

#[derive(Deserialize)]
struct OauthWrapper {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthCreds>,
}

#[derive(Deserialize)]
struct OauthCreds {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct UsageBucketWire {
    utilization: f32,
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucketWire>,
    seven_day: Option<UsageBucketWire>,
}

#[derive(Debug)]
enum CredsError {
    Missing,
    Unreadable,
    Expired,
}

/// Load the raw credentials JSON blob from the platform-appropriate source.
///
/// - macOS: Keychain (service `Claude Code-credentials`, account `$USER`) —
///   this is what Claude Code uses on macOS; there's no JSON file.
/// - Windows / Linux: `<home>/.claude/.credentials.json`.
///
/// Returns `None` when the entry/file is absent or unreadable.
fn load_credentials_blob() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // Shell out to `/usr/bin/security` (Apple-signed, on the partition list
        // of items Claude Code creates) — this avoids the SecurityAgent prompt
        // that direct API access from our unsigned bundle would trigger.
        let Ok(username) = std::env::var("USER") else {
            tracing::warn!("load_credentials_blob: USER env var unset");
            return None;
        };
        let output = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-a",
                &username,
                "-w",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(%stderr, "security find-generic-password failed");
            return None;
        }
        let s = String::from_utf8(output.stdout).ok()?;
        Some(s.trim().to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = resolve_credentials_path()?;
        if !path.exists() {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }
}

/// Persist the credentials JSON blob to the platform-appropriate sink.
fn save_credentials_blob(json: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // `-U` updates if exists, creates otherwise. The JSON is briefly
        // visible via `ps` for the duration of this call (security has no
        // stdin path for passwords) — accepted: the OAuth token is not
        // catastrophic if locally observed, refresh writes happen only on
        // token rotation (~30 days), and Claude Code itself uses the same
        // approach internally.
        let username = std::env::var("USER").map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "USER env var unset")
        })?;
        let output = std::process::Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-s",
                "Claude Code-credentials",
                "-a",
                &username,
                "-w",
                json,
                "-U",
            ])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(std::io::Error::other(format!(
                "security add-generic-password failed: {stderr}"
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let path = resolve_credentials_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no credentials path")
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn resolve_credentials_path() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").ok()?
    } else {
        std::env::var("HOME").ok()?
    };
    Some(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

fn read_credentials() -> Result<String, CredsError> {
    let blob = load_credentials_blob().ok_or(CredsError::Missing)?;
    let wrapper: OauthWrapper =
        serde_json::from_str(&blob).map_err(|_| CredsError::Unreadable)?;
    let creds = wrapper.claude_ai_oauth.ok_or(CredsError::Unreadable)?;
    let token = creds
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or(CredsError::Unreadable)?;
    if let Some(exp) = creds.expires_at {
        if exp > 0 && exp <= now_ms() {
            return Err(CredsError::Expired);
        }
    }
    Ok(token)
}

/// Pull only the refresh token out of the credentials store. Returns None
/// if the entry is missing/unparseable or the refresh_token is absent or
/// blank. Used by the OAuth refresh flow when the access token expires.
fn read_refresh_token() -> Option<String> {
    let blob = load_credentials_blob()?;
    let wrapper: OauthWrapper = serde_json::from_str(&blob).ok()?;
    wrapper
        .claude_ai_oauth?
        .refresh_token
        .filter(|t| !t.trim().is_empty())
}

/// Update accessToken / refreshToken / expiresAt in-place while preserving
/// every other field in the credentials store (scopes, subscriptionType,
/// rateLimitTier, etc.).
fn write_credentials(
    access_token: &str,
    refresh_token: &str,
    expires_at: i64,
) -> std::io::Result<()> {
    let existing = load_credentials_blob().unwrap_or_default();
    let mut value: serde_json::Value =
        serde_json::from_str(&existing).unwrap_or_else(|_| serde_json::json!({}));

    if !value.is_object() {
        value = serde_json::json!({});
    }
    let root = value.as_object_mut().unwrap();
    let oauth_entry = root
        .entry("claudeAiOauth")
        .or_insert_with(|| serde_json::json!({}));
    if !oauth_entry.is_object() {
        *oauth_entry = serde_json::json!({});
    }
    let oauth = oauth_entry.as_object_mut().unwrap();
    oauth.insert(
        "accessToken".into(),
        serde_json::Value::String(access_token.into()),
    );
    oauth.insert(
        "refreshToken".into(),
        serde_json::Value::String(refresh_token.into()),
    );
    oauth.insert("expiresAt".into(), serde_json::Value::from(expires_at));

    let serialized = serde_json::to_string_pretty(&value)?;
    save_credentials_blob(&serialized)
}

// dead_code lint doesn't see Debug-derived reads; fields are consumed via `?err`.
#[allow(dead_code)]
#[derive(Debug)]
enum PollError {
    Auth(reqwest::StatusCode),
    HttpStatus(reqwest::StatusCode, String),
    Network(String),
    JsonParse(String),
}

async fn fetch_usage(
    client: &reqwest::Client,
    token: &str,
) -> Result<UsageResponse, PollError> {
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| PollError::Network(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Err(PollError::Auth(status));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet = body.chars().take(200).collect::<String>();
        return Err(PollError::HttpStatus(status, snippet));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| PollError::Network(e.to_string()))?;
    serde_json::from_str::<UsageResponse>(&body).map_err(|e| {
        let snippet: String = body.chars().take(300).collect();
        PollError::JsonParse(format!("{e}; body={snippet}"))
    })
}

fn to_bucket(wire: UsageBucketWire) -> LimitBucket {
    // Anthropic returns utilization as a 0..100 percentage. The previous
    // "if raw > 1.5 treat as percentage, else fraction" heuristic was meant
    // to defend against an older API shape that returned 0..1 fractions,
    // but it actively misinterpreted real low percentages: a value of 1.0
    // (= 1%) was treated as the fraction 1.0 (= 100%) and the bar showed
    // full red. Always divide by 100; if Anthropic ever flips back to
    // fractions we'll see ~0% bars and revisit.
    LimitBucket {
        utilization: (wire.utilization / 100.0).clamp(0.0, 1.0),
        resets_at: wire.resets_at.map(|t| t.timestamp_millis()),
    }
}

/// Flatten a wire response into a history sample, keeping the raw API
/// percentages (0..100, unclamped) rather than the display-normalized
/// 0..1 values that `to_bucket` produces.
fn to_history_record(usage: &UsageResponse, ts: i64) -> UsageHistoryRecord {
    UsageHistoryRecord {
        ts,
        five_hour_pct: usage.five_hour.as_ref().map(|b| b.utilization),
        five_hour_resets_at: usage
            .five_hour
            .as_ref()
            .and_then(|b| b.resets_at.map(|t| t.timestamp_millis())),
        seven_day_pct: usage.seven_day.as_ref().map(|b| b.utilization),
        seven_day_resets_at: usage
            .seven_day
            .as_ref()
            .and_then(|b| b.resets_at.map(|t| t.timestamp_millis())),
    }
}

pub struct UsageLimitsPoller;

impl UsageLimitsPoller {
    pub fn spawn(app: AppHandle) {
        tauri::async_runtime::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            tracing::info!("usage limits poller started");
            loop {
                poll_once(&app, &client).await;
                let secs = poll_interval_seconds(&app);
                let wake = app
                    .try_state::<UsageLimitsState>()
                    .map(|s| s.wake.clone());
                match wake {
                    Some(wake) => tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(secs)) => {},
                        _ = wake.notified() => {},
                    },
                    None => tokio::time::sleep(Duration::from_secs(secs)).await,
                }
            }
        });
    }
}

pub const MIN_POLL_SECS: u64 = 60;
const REFRESH_COOLDOWN_SECS: u64 = 300;
/// OAuth token endpoint and Claude Code's hardcoded client_id, both used
/// when Claude Code itself refreshes. Discovered via the `claude-code-sdk`
/// gist; if Anthropic ever rotates the client_id we'll need to update this.
const OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

#[derive(Deserialize)]
struct OauthRefreshResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[allow(dead_code)]
#[derive(Debug)]
enum RefreshError {
    NoRefreshToken,
    Network(String),
    HttpStatus(reqwest::StatusCode, String),
    JsonParse(String),
    FileWrite(String),
}

/// Call Anthropic's OAuth token endpoint with the stored refresh_token,
/// then write the rotated token pair back to .credentials.json. Returns
/// the new access token on success.
///
/// Note: Claude Code may refresh concurrently using the same refresh_token.
/// Whichever client calls second gets a 4xx and we return Err — the next
/// poll cycle will re-read the credentials store (now updated by Claude
/// Code's successful refresh) and proceed normally.
async fn do_oauth_refresh(client: &reqwest::Client) -> Result<String, RefreshError> {
    let refresh_token = read_refresh_token().ok_or(RefreshError::NoRefreshToken)?;
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_CODE_CLIENT_ID,
    });
    let resp = client
        .post(OAUTH_TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| RefreshError::Network(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        let snippet: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(RefreshError::HttpStatus(status, snippet));
    }
    let parsed: OauthRefreshResponse = resp
        .json()
        .await
        .map_err(|e| RefreshError::JsonParse(e.to_string()))?;
    let new_expires_at = now_ms() + (parsed.expires_in as i64) * 1000;
    write_credentials(&parsed.access_token, &parsed.refresh_token, new_expires_at)
        .map_err(|e| RefreshError::FileWrite(e.to_string()))?;
    Ok(parsed.access_token)
}

/// Cooldown-gated wrapper around `do_oauth_refresh`. Returns the new
/// access token on success, or None if the cooldown is active or the
/// refresh failed.
async fn try_refresh_token(
    state: &UsageLimitsState,
    client: &reqwest::Client,
) -> Option<String> {
    if !state.claim_refresh_slot() {
        tracing::debug!("token refresh skipped (cooldown active)");
        return None;
    }
    tracing::info!("auth expired; calling OAuth refresh endpoint");
    match do_oauth_refresh(client).await {
        Ok(t) => {
            tracing::info!("token refresh succeeded");
            Some(t)
        }
        Err(e) => {
            tracing::warn!(?e, "token refresh failed");
            None
        }
    }
}

fn poll_interval_seconds(app: &AppHandle) -> u64 {
    app.try_state::<ConfigState>()
        .map(|cfg| cfg.snapshot().usage_limits_poll_interval_seconds)
        .unwrap_or(600)
        .max(MIN_POLL_SECS)
}

async fn poll_once(app: &AppHandle, client: &reqwest::Client) {
    let Some(state) = app.try_state::<UsageLimitsState>() else { return };
    let previous = state.snapshot();
    let now = now_ms();

    let token = match read_credentials() {
        Ok(t) => {
            state.saw_expired_last_poll.store(false, Ordering::SeqCst);
            t
        }
        Err(CredsError::Missing | CredsError::Unreadable) => {
            state.saw_expired_last_poll.store(false, Ordering::SeqCst);
            state.replace(UsageLimits {
                five_hour: None,
                seven_day: None,
                status: UsageStatus::Unavailable,
                updated: now,
            });
            emit_usage_limits_updated(app);
            return;
        }
        Err(CredsError::Expired) => {
            // Defer the first expired sighting to give Claude Code a poll
            // cycle to refresh on its own — avoids racing the rotating
            // refresh_token. Exception: on cold start (no successful poll
            // ever) refresh immediately — the user just launched and we
            // can't make them stare at empty bars for a full poll cycle.
            let was_expired_last = state
                .saw_expired_last_poll
                .swap(true, Ordering::SeqCst);
            let is_cold_start = previous.updated == 0;
            if !was_expired_last && !is_cold_start {
                // Steady-state defer: keep the previous snapshot visible
                // (no replace, no emit) so the user sees no transient
                // disturbance. Next poll will refresh ourselves if CC
                // hasn't done so by then.
                tracing::debug!(
                    "token expired; deferring refresh, keeping previous snapshot"
                );
                return;
            }
            // Either cold start, or this is the second consecutive expired
            // poll. Hit the OAuth endpoint with the stored refresh_token.
            // On failure fall through to AuthExpired so the user knows.
            match try_refresh_token(&state, client).await {
                Some(t) => {
                    state.saw_expired_last_poll.store(false, Ordering::SeqCst);
                    t
                }
                None => {
                    state.replace(UsageLimits {
                        five_hour: None,
                        seven_day: None,
                        status: UsageStatus::AuthExpired,
                        updated: now,
                    });
                    emit_usage_limits_updated(app);
                    return;
                }
            }
        }
    };

    let mut result = fetch_usage(client, &token).await;
    if matches!(result, Err(PollError::Auth(_))) {
        // Server rejected even though our local expiresAt said the token
        // was fresh — try a forced refresh and retry once.
        if let Some(new_token) = try_refresh_token(&state, client).await {
            result = fetch_usage(client, &new_token).await;
        }
    }

    match result {
        Ok(usage) => {
            tracing::debug!(
                five_hour_raw = ?usage.five_hour.as_ref().map(|b| b.utilization),
                seven_day_raw = ?usage.seven_day.as_ref().map(|b| b.utilization),
                "usage poll success"
            );
            if let Some(history) = app.try_state::<UsageHistoryStore>() {
                history.append(&to_history_record(&usage, now));
            }
            state.replace(UsageLimits {
                five_hour: usage.five_hour.map(to_bucket),
                seven_day: usage.seven_day.map(to_bucket),
                status: UsageStatus::Ok,
                updated: now,
            });
            emit_usage_limits_updated(app);
        }
        Err(err) => {
            let status = match err {
                PollError::Auth(_) => UsageStatus::AuthExpired,
                _ => UsageStatus::NetworkError,
            };
            tracing::warn!(?err, "usage limits poll failed");
            // Keep last-known buckets visible on transient failures; the
            // tooltip's "updated Ns ago" carries the staleness signal.
            let keep_prev = matches!(status, UsageStatus::NetworkError)
                && previous.five_hour.is_some();
            state.replace(UsageLimits {
                five_hour: if keep_prev { previous.five_hour.clone() } else { None },
                seven_day: if keep_prev { previous.seven_day.clone() } else { None },
                status,
                updated: if keep_prev { previous.updated } else { now },
            });
            emit_usage_limits_updated(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    /// Serializes tests that override the per-platform credentials env var
    /// (`USERPROFILE` on Windows, `HOME` elsewhere) — cargo test runs them in
    /// parallel by default and a global env var is shared across threads.
    /// macOS path goes through the Keychain, so credentials path tests are
    /// already gated `#[cfg(not(target_os = "macos"))]`.
    #[cfg(not(target_os = "macos"))]
    fn cred_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        // Poison the lock if a prior test panicked; we still want to run.
        match M.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    /// Point credentials resolution at a test-owned `<temp>/.claude/`. Returns
    /// `(temp_root, credentials_path, restore_guard)` — drop the guard to put
    /// the env var back. Caller must hold [`cred_test_lock`].
    #[cfg(not(target_os = "macos"))]
    fn override_credentials_home(tag: &str) -> (PathBuf, PathBuf, EnvVarGuard) {
        let temp = std::env::temp_dir().join(format!(
            "claude_code_dashboard_usage_test_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(".claude")).unwrap();
        let prior = std::env::var(HOME_VAR).ok();
        // SAFETY: tests serialized via cred_test_lock() ensure no concurrent reads.
        unsafe { std::env::set_var(HOME_VAR, &temp) };
        let creds = temp.join(".claude").join(".credentials.json");
        (
            temp.clone(),
            creds,
            EnvVarGuard {
                key: HOME_VAR,
                prior,
                cleanup: Some(temp),
            },
        )
    }

    #[cfg(not(target_os = "macos"))]
    struct EnvVarGuard {
        key: &'static str,
        prior: Option<String>,
        cleanup: Option<PathBuf>,
    }

    #[cfg(not(target_os = "macos"))]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests serialized via cred_test_lock() ensure no concurrent reads.
            unsafe {
                match self.prior.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
            if let Some(p) = self.cleanup.take() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
    }

    #[test]
    fn parses_happy_credentials() {
        let bytes = br#"{
            "claudeAiOauth": {
                "accessToken": "sk-xxx",
                "refreshToken": "r",
                "expiresAt": 9999999999999
            }
        }"#;
        let w: OauthWrapper = serde_json::from_slice(bytes).unwrap();
        let creds = w.claude_ai_oauth.unwrap();
        assert_eq!(creds.access_token.as_deref(), Some("sk-xxx"));
        assert_eq!(creds.expires_at, Some(9999999999999));
    }

    #[test]
    fn tolerates_missing_oauth_block() {
        let bytes = br#"{"otherField": 1}"#;
        let w: OauthWrapper = serde_json::from_slice(bytes).unwrap();
        assert!(w.claude_ai_oauth.is_none());
    }

    #[test]
    fn request_refresh_respects_min_poll_floor() {
        let state = UsageLimitsState::new();
        // Seed updated 30s ago — inside the 60s floor; refresh is dropped.
        state.replace(UsageLimits {
            five_hour: None,
            seven_day: None,
            status: UsageStatus::Ok,
            updated: now_ms() - 30_000,
        });
        assert!(!state.request_refresh());

        // Seed updated 61s ago — outside the floor; refresh wakes the poller.
        state.replace(UsageLimits {
            five_hour: None,
            seven_day: None,
            status: UsageStatus::Ok,
            updated: now_ms() - 61_000,
        });
        assert!(state.request_refresh());
    }

    #[test]
    fn deserializes_usage_response() {
        let bytes = br#"{
            "five_hour":  { "utilization": 0.42, "resets_at": "2026-04-20T22:00:00.000+00:00" },
            "seven_day":  { "utilization": 0.18, "resets_at": "2026-04-25T00:00:00Z" }
        }"#;
        let r: UsageResponse = serde_json::from_slice(bytes).unwrap();
        let fh = r.five_hour.unwrap();
        assert!((fh.utilization - 0.42).abs() < 1e-6);
        let expected = DateTime::parse_from_rfc3339("2026-04-20T22:00:00.000+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(fh.resets_at, Some(expected));

        let sd = r.seven_day.unwrap();
        assert!((sd.utilization - 0.18).abs() < 1e-6);
    }

    #[test]
    fn tolerates_null_resets_at() {
        // Anthropic returns resets_at: null for buckets that have no active
        // window (typically utilization 0). The whole response must still parse.
        let bytes = br#"{
            "five_hour": { "utilization": 0.0, "resets_at": null },
            "seven_day": { "utilization": 79.0, "resets_at": "2026-04-23T20:59:59.823528+00:00" }
        }"#;
        let r: UsageResponse = serde_json::from_slice(bytes).unwrap();
        let fh = r.five_hour.unwrap();
        assert_eq!(fh.utilization, 0.0);
        assert!(fh.resets_at.is_none());
        let sd = r.seven_day.unwrap();
        assert!(sd.resets_at.is_some());

        let fh_bucket = to_bucket(fh);
        assert!(fh_bucket.resets_at.is_none());
    }

    #[test]
    fn to_bucket_rescales_percentage_scale() {
        let wire = UsageBucketWire {
            utilization: 42.0,
            resets_at: Some(Utc::now()),
        };
        let b = to_bucket(wire);
        assert!((b.utilization - 0.42).abs() < 1e-6);
    }

    #[test]
    fn to_bucket_handles_low_percentages() {
        // Regression: a raw value of 1.0 means 1%, not 100%. The old
        // "fraction vs percentage" heuristic mistook this for a fraction.
        let wire = UsageBucketWire { utilization: 1.0, resets_at: None };
        let b = to_bucket(wire);
        assert!((b.utilization - 0.01).abs() < 1e-6);
    }

    #[test]
    fn to_history_record_keeps_raw_percentages() {
        let resets = DateTime::parse_from_rfc3339("2026-04-20T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let usage = UsageResponse {
            five_hour: Some(UsageBucketWire {
                utilization: 42.5,
                resets_at: Some(resets),
            }),
            seven_day: None,
        };
        let rec = to_history_record(&usage, 1234);
        assert_eq!(rec.ts, 1234);
        assert_eq!(rec.five_hour_pct, Some(42.5));
        assert_eq!(rec.five_hour_resets_at, Some(resets.timestamp_millis()));
        assert_eq!(rec.seven_day_pct, None);
        assert_eq!(rec.seven_day_resets_at, None);
    }

    #[test]
    fn to_bucket_clamps_out_of_range() {
        let wire = UsageBucketWire {
            utilization: 150.0,
            resets_at: Some(Utc::now()),
        };
        let b = to_bucket(wire);
        assert!((b.utilization - 1.0).abs() < 1e-6);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn missing_file_returns_missing() {
        let _g = cred_test_lock();
        let (_temp, _path, _restore) = override_credentials_home("missing");
        assert!(matches!(read_credentials(), Err(CredsError::Missing)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn expired_token_returns_expired() {
        let _g = cred_test_lock();
        let (_temp, path, _restore) = override_credentials_home("expired");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{"claudeAiOauth":{{"accessToken":"x","expiresAt":100}}}}"#
        )
        .unwrap();
        drop(f);
        assert!(matches!(read_credentials(), Err(CredsError::Expired)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn valid_token_returned() {
        let _g = cred_test_lock();
        let (_temp, path, _restore) = override_credentials_home("valid");
        let mut f = std::fs::File::create(&path).unwrap();
        let future = now_ms() + 60 * 60 * 1000;
        write!(
            f,
            r#"{{"claudeAiOauth":{{"accessToken":"tok","expiresAt":{future}}}}}"#
        )
        .unwrap();
        drop(f);
        assert_eq!(read_credentials().unwrap(), "tok");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn write_credentials_preserves_unrelated_fields() {
        let _g = cred_test_lock();
        let (_temp, path, _restore) = override_credentials_home("write");
        std::fs::write(
            &path,
            r#"{
              "claudeAiOauth": {
                "accessToken": "old",
                "refreshToken": "old_r",
                "expiresAt": 1,
                "scopes": ["a", "b"],
                "subscriptionType": "max"
              },
              "otherTopLevel": 42
            }"#,
        )
        .unwrap();

        write_credentials("new_a", "new_r", 9_999_999_999_999).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let oauth = written.get("claudeAiOauth").unwrap();
        assert_eq!(oauth.get("accessToken").unwrap(), "new_a");
        assert_eq!(oauth.get("refreshToken").unwrap(), "new_r");
        assert_eq!(oauth.get("expiresAt").unwrap(), 9_999_999_999_999i64);
        // Untouched fields survive.
        assert_eq!(oauth.get("scopes").unwrap(), &serde_json::json!(["a", "b"]));
        assert_eq!(oauth.get("subscriptionType").unwrap(), "max");
        assert_eq!(written.get("otherTopLevel").unwrap(), 42);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn read_refresh_token_extracts_when_present() {
        let _g = cred_test_lock();
        let (_temp, path, _restore) = override_credentials_home("refresh_read");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r-123","expiresAt":1}}"#,
        )
        .unwrap();
        assert_eq!(read_refresh_token().as_deref(), Some("r-123"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn blank_token_treated_as_unreadable() {
        let _g = cred_test_lock();
        let (_temp, path, _restore) = override_credentials_home("blank");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"{{"claudeAiOauth":{{"accessToken":"  "}}}}"#).unwrap();
        drop(f);
        assert!(matches!(read_credentials(), Err(CredsError::Unreadable)));
    }
}
