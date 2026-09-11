//! The last good usage reading, and the moment the endpoint said to come back.
//!
//! Both facts have to outlive the process, and for the same reason: this app is
//! restarted constantly (every deploy is a restart), and an in-memory copy of
//! either one turns every restart into a fresh request against an endpoint that
//! had already answered. Measured on this machine, a launch used to fire two
//! requests at once and a development session's worth of launches put the
//! endpoint into a multi-hour refusal.
//!
//! Its own file rather than a `config.json` field, for the reason
//! `custom_names.json` has one: the deploy step overwrites `config.json` from
//! the repo's template, so a deadline written there would be erased by exactly
//! the event it exists to survive.
//!
//! `GET /api/oauth/usage` is undocumented — it appears nowhere in Anthropic's
//! documentation — but a real 429 from it on 2026-09-10 carried
//! `retry-after: 832`, so the endpoint does say how long to wait. Captures in
//! the wild have also shown `retry-after: 0` and none at all, which is why a
//! non-positive value is treated as absent rather than as "retry now".

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::usage_limits::{LimitBucket, UsageLimits, UsageStatus};

/// Refuse a deadline further out than this. A garbage `Retry-After` would
/// otherwise disable the usage bars for as long as the file survives, and the
/// user has no way to see why.
const MAX_BACKOFF_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Default, Serialize, Deserialize)]
struct CacheFile {
    /// The newest reading that actually succeeded, replayed at startup so the
    /// bars show a real figure with an age instead of `--%`.
    #[serde(default)]
    five_hour: Option<LimitBucket>,
    #[serde(default)]
    seven_day: Option<LimitBucket>,
    #[serde(default)]
    updated: i64,
    /// Epoch ms before which no request may be made, from the endpoint's own
    /// `Retry-After`. Absent means nothing is owed.
    #[serde(default)]
    blocked_until: Option<i64>,
}

pub struct UsageCacheStore {
    path: PathBuf,
    data: Mutex<CacheFile>,
}

impl UsageCacheStore {
    pub fn new(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<CacheFile>(&s).ok())
            .unwrap_or_default();
        tracing::debug!(
            has_sample = data.updated != 0,
            blocked_until = ?data.blocked_until,
            "usage cache loaded"
        );
        Self { path, data: Mutex::new(data) }
    }

    /// The stored reading as a snapshot, or `None` when nothing was ever stored.
    ///
    /// The caller supplies the `status`, because the same bytes mean different
    /// things depending on why they are being replayed and only the caller knows
    /// which: a sample replayed while the endpoint is refusing is stale and says
    /// `NetworkError`, while one replayed instead of an opening poll it would
    /// have duplicated is as good as any reading in steady state -- it is younger
    /// than the poll interval, which is the most any displayed figure ever is --
    /// and says `Ok`. Baking either one in here would make the other a lie, and
    /// `status` is not decoration: the notifier's reset tracker seeds from `Ok`
    /// readings only.
    ///
    /// `updated` is always the moment the reading was TAKEN, never now, so the
    /// age the UI shows stays honest however it is replayed.
    pub fn replay(&self, status: UsageStatus) -> Option<UsageLimits> {
        let d = self.data.lock().unwrap();
        if d.updated == 0 || d.five_hour.is_none() {
            return None;
        }
        Some(UsageLimits {
            five_hour: d.five_hour.clone(),
            seven_day: d.seven_day.clone(),
            status,
            updated: d.updated,
        })
    }

    pub fn store_sample(&self, usage: &UsageLimits) {
        {
            let mut d = self.data.lock().unwrap();
            d.five_hour = usage.five_hour.clone();
            d.seven_day = usage.seven_day.clone();
            d.updated = usage.updated;
            // A reading means the endpoint is answering again, so any deadline
            // it set is spent. This is the only thing that clears one early --
            // there is deliberately no separate clear method, since nothing but
            // a successful poll or the clock can know the refusal is over.
            d.blocked_until = None;
        }
        self.save();
    }

    /// Record how long the endpoint asked to be left alone. `secs` is whatever
    /// `Retry-After` carried; a non-positive value means it told us nothing.
    pub fn block_for(&self, secs: i64, now_ms: i64) -> Option<i64> {
        if secs <= 0 {
            return None;
        }
        let until = now_ms + (secs * 1000).min(MAX_BACKOFF_MS);
        {
            let mut d = self.data.lock().unwrap();
            d.blocked_until = Some(until);
        }
        self.save();
        Some(until)
    }

    /// Epoch ms to wait until, when a deadline is still in the future.
    pub fn blocked_until(&self, now_ms: i64) -> Option<i64> {
        let d = self.data.lock().unwrap();
        d.blocked_until.filter(|until| *until > now_ms)
    }

    fn save(&self) {
        let d = self.data.lock().unwrap();
        match serde_json::to_string_pretty(&*d) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write usage cache");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize usage cache"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UsageCacheStore {
        let mut p = std::env::temp_dir();
        p.push(format!("ccdash-usage-cache-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        UsageCacheStore::new(p)
    }

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn a_non_positive_retry_after_blocks_nothing() {
        // Observed in the wild on this route. Taken at face value it produces a
        // deadline already in the past, i.e. no backoff at all -- so it has to
        // mean "told us nothing", not "retry now".
        let s = store();
        assert_eq!(s.block_for(0, NOW), None);
        assert_eq!(s.block_for(-5, NOW), None);
        assert_eq!(s.blocked_until(NOW), None);
    }

    #[test]
    fn a_deadline_survives_and_expires() {
        let s = store();
        assert_eq!(s.block_for(832, NOW), Some(NOW + 832_000));
        assert_eq!(s.blocked_until(NOW), Some(NOW + 832_000));
        assert_eq!(s.blocked_until(NOW + 832_001), None, "expired deadlines are not blocking");
    }

    #[test]
    fn a_wild_retry_after_is_capped() {
        let s = store();
        let until = s.block_for(999_999_999, NOW).unwrap();
        assert_eq!(until, NOW + MAX_BACKOFF_MS, "a garbage value must not brick the bars");
    }

    #[test]
    fn a_stored_sample_replays_as_stale_rather_than_fresh() {
        let s = store();
        let sample = UsageLimits {
            five_hour: Some(LimitBucket { utilization: 40.0, resets_at: Some(NOW + 3_600_000) }),
            seven_day: None,
            status: UsageStatus::Ok,
            updated: NOW,
        };
        s.store_sample(&sample);
        let back = s.replay(UsageStatus::NetworkError).expect("a stored sample replays");
        assert_eq!(back.updated, NOW, "the age of the reading is preserved");
        assert_eq!(back.status, UsageStatus::NetworkError, "replayed, so not claimed fresh");
        assert!(back.five_hour.is_some());

        // The same bytes, replayed for the other reason, are a good reading --
        // and the notifier's reset tracker seeds from `Ok` only, so this is not
        // cosmetic.
        let fresh = s.replay(UsageStatus::Ok).expect("a stored sample replays");
        assert_eq!(fresh.status, UsageStatus::Ok);
        assert_eq!(fresh.updated, NOW, "still the moment it was taken, never now");
    }

    #[test]
    fn nothing_stored_replays_nothing() {
        assert!(store().replay(UsageStatus::Ok).is_none());
    }
}
