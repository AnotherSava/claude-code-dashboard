use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One sample of the Anthropic usage-limits poll, flattened for easy
/// time-series processing (each JSONL line is effectively a named-column
/// CSV row). Percentages are stored raw as the API returns them (0..100,
/// unclamped) — graphing code decides how to normalize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageHistoryRecord {
    /// Poll timestamp, ms since epoch.
    pub ts: i64,
    pub five_hour_pct: Option<f32>,
    /// Window reset time, ms since epoch.
    pub five_hour_resets_at: Option<i64>,
    pub seven_day_pct: Option<f32>,
    pub seven_day_resets_at: Option<i64>,
}

/// Append-only JSONL sink for usage-limit samples (`usage_history.jsonl`
/// in the app data dir). Lives in its own file rather than config.json so
/// it survives the deploy step that overwrites config.json.
pub struct UsageHistoryStore {
    path: PathBuf,
    // Serializes appends; polls are sequential today, but the lock makes
    // that a property of the store rather than of its single caller.
    write_lock: Mutex<()>,
}

impl UsageHistoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, write_lock: Mutex::new(()) }
    }

    pub fn append(&self, record: &UsageHistoryRecord) {
        let line = match serde_json::to_string(record) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(?e, "failed to serialize usage history record");
                return;
            }
        };
        let _guard = self.write_lock.lock().unwrap();
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            tracing::warn!(?e, path = %self.path.display(), "failed to append usage history");
        }
    }

    /// Read and parse every JSONL line, dropping malformed ones. Returns
    /// records sorted ascending by `ts` — the file is appended in poll order,
    /// but sorting is cheap insurance against a clock step back corrupting the
    /// consecutive-delta walk in `build_weekly_quota`. A missing file is an empty
    /// history, not an error. Re-reading per call is trivially cheap at the
    /// current scale (a few thousand lines), so there's no cache.
    pub fn read_all(&self) -> Vec<UsageHistoryRecord> {
        let _guard = self.write_lock.lock().unwrap();
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!(?e, path = %self.path.display(), "failed to read usage history");
                return Vec::new();
            }
        };
        let mut records: Vec<UsageHistoryRecord> = contents
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        records.sort_by_key(|r| r.ts);
        records
    }
}

/// The 10-minute time grid the Work intensity chart is laid out on. It is
/// defined here, beside the week roll-up that shares it, while the bars that
/// sit on it live in [`crate::token_history`] — the percent bars this module
/// used to build were removed once the chart became token-only, and only the
/// per-day quota numbers below survived.
pub const BUCKET_MS: i64 = 10 * 60 * 1000;
pub const BUCKETS_PER_DAY: usize = 6 * 24; // 144
pub const BUCKETS_PER_WEEK: usize = BUCKETS_PER_DAY * 7; // 1008
pub const DAY_MS: i64 = BUCKET_MS * BUCKETS_PER_DAY as i64;
pub const WEEK_MS: i64 = BUCKET_MS * BUCKETS_PER_WEEK as i64;

/// Percent of the 7-day (weekly) quota consumed on each day (Mon..Sun) of the
/// week starting at `week_start_ms`: the positive increments of
/// `seven_day_pct`, time-weighted across the days an interval spans. Records
/// are assumed sorted ascending by `ts`.
///
/// A weekly-window reset is a pct drop that clamps to 0, so a reset day still
/// totals only the genuine consumption on either side of the reset, never a
/// negative. `seven_day_resets_at` is deliberately not consulted: it jitters by
/// ±1 min between polls with no real reset, which would mis-attribute the
/// absolute percentage on every step.
///
/// Inter-observation gaps are **not** excluded, and that is why these numbers
/// outlived the percent *bars* they used to be shown beside. `seven_day_pct` is
/// a slow cumulative counter, so the rise across a span this app did not
/// observe is real account usage and belongs to the day(s) it covers — where a
/// per-10-min 5h delta across the same span could only have been invented.
///
/// Pure: no clock, no timezone — all tz/DST/week-alignment logic lives in the
/// calling command, so this is fully unit-testable with synthetic records.
pub fn build_weekly_quota(records: &[UsageHistoryRecord], week_start_ms: i64) -> [f32; 7] {
    let week_end_ms = week_start_ms + WEEK_MS;
    let mut weekly = [0.0f32; 7];
    for pair in records.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let (Some(ps), Some(cs)) = (prev.seven_day_pct, cur.seven_day_pct) else {
            continue;
        };
        let dt = cur.ts - prev.ts;
        if dt <= 0 {
            continue;
        }
        let delta = (cs - ps).max(0.0);
        if delta <= 0.0 {
            continue;
        }
        let start = prev.ts.max(week_start_ms);
        let end = cur.ts.min(week_end_ms);
        if start >= end {
            continue;
        }
        let rate = delta / dt as f32;
        let first_day = ((start - week_start_ms) / DAY_MS) as usize;
        let last_day = ((end - 1 - week_start_ms) / DAY_MS) as usize;
        for d in first_day..=last_day {
            let d_start = week_start_ms + d as i64 * DAY_MS;
            let overlap = end.min(d_start + DAY_MS) - start.max(d_start);
            if overlap > 0 {
                weekly[d] += rate * overlap as f32;
            }
        }
    }

    weekly
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (PathBuf, UsageHistoryStore) {
        let path = std::env::temp_dir().join(format!(
            "claude_code_dashboard_usage_history_test_{}_{}.jsonl",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (path.clone(), UsageHistoryStore::new(path))
    }

    #[test]
    fn appends_one_line_per_record() {
        let (path, store) = temp_store("append");
        store.append(&UsageHistoryRecord {
            ts: 1,
            five_hour_pct: Some(42.5),
            five_hour_resets_at: Some(1000),
            seven_day_pct: Some(18.0),
            seven_day_resets_at: None,
        });
        store.append(&UsageHistoryRecord {
            ts: 2,
            five_hour_pct: None,
            five_hour_resets_at: None,
            seven_day_pct: Some(19.0),
            seven_day_resets_at: Some(2000),
        });

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: UsageHistoryRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.ts, 1);
        assert_eq!(first.five_hour_pct, Some(42.5));
        assert_eq!(first.seven_day_resets_at, None);
        let second: UsageHistoryRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.ts, 2);
        assert_eq!(second.five_hour_pct, None);
        assert_eq!(second.seven_day_resets_at, Some(2000));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_survives_unwritable_path() {
        // A directory path can't be opened as a file — append must log and
        // return rather than panic, so a bad path never disturbs polling.
        let store = UsageHistoryStore::new(std::env::temp_dir());
        store.append(&UsageHistoryRecord {
            ts: 1,
            five_hour_pct: None,
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
        });
    }

    #[test]
    fn read_all_sorts_and_round_trips() {
        let (path, store) = temp_store("read_all");
        // Appended out of order to prove read_all sorts ascending by ts.
        store.append(&UsageHistoryRecord {
            ts: 2,
            five_hour_pct: Some(9.0),
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
        });
        store.append(&UsageHistoryRecord {
            ts: 1,
            five_hour_pct: Some(8.0),
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
        });
        let records = store.read_all();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].ts, 1);
        assert_eq!(records[1].ts, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_all_missing_file_is_empty() {
        let (_path, store) = temp_store("missing");
        assert!(store.read_all().is_empty());
    }

    // --- build_weekly_quota -----------------------------------------------

    /// Week start aligned to a bucket boundary (WEEK_MS is a multiple of
    /// BUCKET_MS), so `idx = (ts - WK) / BUCKET_MS` is exact in assertions.
    const WK: i64 = 100 * WEEK_MS;

    fn full_rec(ts: i64, pct: f32, seven: f32, seven_resets: i64) -> UsageHistoryRecord {
        UsageHistoryRecord {
            ts,
            five_hour_pct: Some(pct),
            five_hour_resets_at: Some(1),
            seven_day_pct: Some(seven),
            seven_day_resets_at: Some(seven_resets),
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn weekly_quota_sums_positive_increments() {
        // seven_day climbs 20 -> 22 -> 23 on Monday = +3% of the weekly quota,
        // and no other day is touched.
        let recs = [
            full_rec(WK, 10.0, 20.0, 100),
            full_rec(WK + BUCKET_MS, 13.0, 22.0, 100),
            full_rec(WK + 2 * BUCKET_MS, 16.0, 23.0, 100),
        ];
        let weekly = build_weekly_quota(&recs, WK);
        assert!(close(weekly[0], 3.0));
        assert!(weekly[1..].iter().all(|&p| close(p, 0.0)));
    }

    #[test]
    fn weekly_pct_clamps_on_seven_day_reset() {
        // The weekly window resets mid-day: seven_day drops 80 -> 5. The day
        // totals only the post-reset rise (here 0 across the single drop
        // interval), never the -75.
        let recs = [
            full_rec(WK, 10.0, 80.0, 100),
            full_rec(WK + BUCKET_MS, 11.0, 5.0, 200),
        ];
        let weekly = build_weekly_quota(&recs, WK);
        assert!(weekly[0] >= 0.0);
        assert!(close(weekly[0], 0.0));
    }

    #[test]
    fn weekly_pct_splits_across_midnight() {
        // An interval spanning Mon 23:50 -> Tue 00:00+ splits its seven_day
        // delta across the two days by overlap.
        let recs = [
            full_rec(WK + DAY_MS - 5 * 60 * 1000, 10.0, 10.0, 100), // 5 min before midnight
            full_rec(WK + DAY_MS + 5 * 60 * 1000, 11.0, 14.0, 100), // 5 min after
        ];
        let weekly = build_weekly_quota(&recs, WK);
        assert!(close(weekly[0], 2.0));
        assert!(close(weekly[1], 2.0));
    }

    #[test]
    fn unobserved_gap_is_still_attributed() {
        // The counterpart of the deleted 5h-intensity gap rule: a 5h stretch the
        // poller never covered still rose by a real 10% of the weekly quota, so
        // it is attributed rather than discarded.
        let five_h = 5 * 3600 * 1000;
        let recs = [full_rec(WK, 10.0, 20.0, 100), full_rec(WK + five_h, 11.0, 30.0, 100)];
        let weekly = build_weekly_quota(&recs, WK);
        assert!(close(weekly[0], 10.0));
    }

    #[test]
    fn empty_input_is_all_zero() {
        assert!(build_weekly_quota(&[], WK).iter().all(|&p| p == 0.0));
    }

    #[test]
    fn records_outside_week_contribute_nothing() {
        let recs = [
            full_rec(WK - 10 * BUCKET_MS, 10.0, 20.0, 100),
            full_rec(WK - 9 * BUCKET_MS, 13.0, 23.0, 100),
        ];
        assert!(build_weekly_quota(&recs, WK).iter().all(|&p| p == 0.0));
    }

    #[test]
    fn interval_straddling_week_start_counts_only_in_week_fraction() {
        // Half a bucket before the week to half a bucket after: dt = 1 bucket,
        // Δ2, only the in-week half (1.0) lands on Monday.
        let half = BUCKET_MS / 2;
        let recs = [
            full_rec(WK - half, 10.0, 10.0, 100),
            full_rec(WK + half, 12.0, 12.0, 100),
        ];
        assert!(close(build_weekly_quota(&recs, WK)[0], 1.0));
    }

    #[test]
    fn none_seven_day_pct_between_valid_is_skipped() {
        let recs = [
            full_rec(WK, 10.0, 20.0, 100),
            UsageHistoryRecord {
                ts: WK + BUCKET_MS,
                five_hour_pct: Some(13.0),
                five_hour_resets_at: Some(1),
                seven_day_pct: None,
                seven_day_resets_at: None,
            },
            full_rec(WK + 2 * BUCKET_MS, 16.0, 23.0, 100),
        ];
        // Both pairs touch the None record, so nothing is attributed.
        assert!(build_weekly_quota(&recs, WK).iter().all(|&p| p == 0.0));
    }
}
