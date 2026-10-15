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
/// negative. `seven_day_resets_at` is deliberately not consulted: it jitters
/// between polls with no real reset (see [`RESET_JITTER_MS`]), which would
/// mis-attribute the absolute percentage on every step.
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

/// The 7-day limit percentage in force during each 10-minute bucket of the
/// week starting at `week_start_ms`, which colours that bucket's bar. Records
/// are assumed sorted ascending by `ts`.
///
/// A bucket holding samples takes the highest of them. A bucket holding none
/// takes the last sample before it, because `seven_day_pct` only rises inside a
/// window, so the carried value is a lower bound and never overstates how close
/// the limit was. A mid-window reset leaves `seven_day_resets_at` unchanged, so
/// the bucket it falls in keeps the spent window's readings and takes their
/// highest; the Weeks view's grouping skips that bucket for this reason.
///
/// A sample counts for a bucket only while its own window is still open at the
/// bucket's start, judged against `seven_day_resets_at` with a minute of
/// slack for that timestamp's jitter. That ends the carry at the reset, and it
/// keeps out the poll that lands in the first bucket of a new window still
/// reporting the old one (observed 53 ms after a reset, at 100%), which would
/// otherwise colour that bucket with the spent window. `None` where nothing
/// applies.
pub fn build_bucket_limit_pct(records: &[UsageHistoryRecord], week_start_ms: i64) -> Vec<Option<f32>> {
    let open_at = |r: &UsageHistoryRecord, b_start: i64| r.seven_day_resets_at.is_none_or(|reset| b_start + RESET_JITTER_MS < reset);
    let mut out = vec![None; BUCKETS_PER_WEEK];
    let mut i = records.partition_point(|r| r.ts < week_start_ms);
    let mut carry = records[..i].iter().rev().find(|r| r.seven_day_pct.is_some());
    for (idx, slot) in out.iter_mut().enumerate() {
        let b_start = week_start_ms + idx as i64 * BUCKET_MS;
        let b_end = b_start + BUCKET_MS;
        let mut inside: Option<f32> = None;
        while i < records.len() && records[i].ts < b_end {
            if let Some(pct) = records[i].seven_day_pct.filter(|_| open_at(&records[i], b_start)) {
                inside = Some(inside.map_or(pct, |m| m.max(pct)));
                carry = Some(&records[i]);
            }
            i += 1;
        }
        *slot = inside.or_else(|| carry.filter(|c| open_at(c, b_start)).and_then(|c| c.seven_day_pct));
    }
    out
}

/// Slack for the jitter `seven_day_resets_at` shows between polls of one
/// window, measured at under 3 s across June–October 2026.
const RESET_JITTER_MS: i64 = 60 * 1000;

/// How far `seven_day_resets_at` must move to count as another window rather
/// than [`RESET_JITTER_MS`]'s jitter. Also the span inside which two sightings
/// of a reset are taken to be one.
const RESET_JUMP_MS: i64 = 60 * 60 * 1000;

/// A `seven_day_pct` reading at or below this, at least [`RESET_DROP_PCT`]
/// under the one before it, is a reset.
const RESET_FLOOR_PCT: f32 = 5.0;
const RESET_DROP_PCT: f32 = 10.0;

/// Every 7-day window reset the samples witnessed, ascending, in ms. Records
/// are assumed sorted by `ts`. Two signals, because neither sees every reset:
///
/// - `seven_day_resets_at` moving forward. The window it moved away from ended
///   at its own scheduled time, or at the sample that saw the new one if that
///   came first. This is the only trace of a scheduled reset after a week with
///   nothing used, where the percentage has nowhere to drop from.
/// - `seven_day_pct` falling to near zero. The account's weekly usage is also
///   reset mid-window, leaving `seven_day_resets_at` where it was — ten times
///   between June and October 2026. The first such reading often carries a
///   null `seven_day_resets_at`; it is still the reset, since in every case on
///   record the percentage stayed low afterwards.
///
/// A scheduled reset usually trips both, so sightings within [`RESET_JUMP_MS`]
/// of each other are one reset at the earlier instant. Only witnessed resets
/// are reported: one inside a stretch with no samples leaves no trace and is
/// not extrapolated from the weekly schedule. A `seven_day_resets_at` that
/// moves backwards is a peer still carrying the old window, and the whole
/// record is ignored, its percentage included.
pub fn seven_day_resets(records: &[UsageHistoryRecord]) -> Vec<i64> {
    let mut seen = Vec::new();
    let mut window_end: Option<i64> = None;
    let mut last_pct: Option<f32> = None;
    for r in records {
        if let Some(at) = r.seven_day_resets_at {
            match window_end {
                Some(prev) if at > prev + RESET_JUMP_MS => {
                    seen.push(prev.min(r.ts));
                    window_end = Some(at);
                }
                Some(prev) if at + RESET_JUMP_MS < prev => continue,
                Some(_) => {}
                None => window_end = Some(at),
            }
        }
        if let Some(pct) = r.seven_day_pct {
            if last_pct.is_some_and(|last| pct <= RESET_FLOOR_PCT && pct + RESET_DROP_PCT <= last) {
                seen.push(r.ts);
            }
            last_pct = Some(pct);
        }
    }
    seen.sort_unstable();
    let mut out: Vec<i64> = Vec::new();
    for t in seen {
        if out.last().is_none_or(|&prev| t > prev + RESET_JUMP_MS) {
            out.push(t);
        }
    }
    out
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
    #[test]
    fn bucket_limit_takes_the_highest_sample_inside_a_bucket() {
        let recs = vec![full_rec(WK + 1_000, 0.0, 81.0, WK + WEEK_MS), full_rec(WK + 2_000, 0.0, 83.0, WK + WEEK_MS)];
        assert_eq!(build_bucket_limit_pct(&recs, WK)[0], Some(83.0));
    }

    #[test]
    fn bucket_limit_carries_the_last_sample_until_its_reset() {
        // Sampled before the week, resetting three buckets in: the first three
        // buckets carry it, the rest have no reading.
        let recs = vec![full_rec(WK - BUCKET_MS, 0.0, 95.0, WK + 3 * BUCKET_MS)];
        let out = build_bucket_limit_pct(&recs, WK);
        assert_eq!(&out[..4], &[Some(95.0), Some(95.0), Some(95.0), None]);
    }

    #[test]
    fn bucket_limit_is_none_before_any_sample() {
        let recs = vec![full_rec(WK + 5 * BUCKET_MS, 0.0, 40.0, WK + WEEK_MS)];
        let out = build_bucket_limit_pct(&recs, WK);
        assert_eq!(out[4], None);
        assert_eq!(out[5], Some(40.0));
        assert_eq!(out[6], Some(40.0));
    }

    #[test]
    fn a_reset_is_the_window_end_the_samples_moved_past() {
        let end = WK + DAY_MS;
        let recs = vec![
            full_rec(WK, 0.0, 50.0, end),
            full_rec(WK + BUCKET_MS, 0.0, 51.0, end + 30_000), // jitter, same window
            full_rec(end + BUCKET_MS, 0.0, 0.0, end + WEEK_MS),
        ];
        assert_eq!(seven_day_resets(&recs), vec![end]);
    }

    #[test]
    fn an_early_rollover_resets_at_the_sample_that_saw_it() {
        let recs = vec![full_rec(WK, 0.0, 50.0, WK + WEEK_MS), full_rec(WK + DAY_MS, 0.0, 0.0, WK + DAY_MS + WEEK_MS)];
        assert_eq!(seven_day_resets(&recs), vec![WK + DAY_MS]);
    }

    #[test]
    fn a_mid_window_drop_to_zero_is_a_reset() {
        let end = WK + WEEK_MS;
        let recs = vec![
            full_rec(WK, 0.0, 100.0, end),
            UsageHistoryRecord { seven_day_resets_at: None, ..full_rec(WK + DAY_MS, 0.0, 0.0, 0) },
            full_rec(WK + DAY_MS + BUCKET_MS, 0.0, 1.0, end),
        ];
        assert_eq!(seven_day_resets(&recs), vec![WK + DAY_MS]);
    }

    #[test]
    fn a_small_dip_is_not_a_reset() {
        let end = WK + WEEK_MS;
        let recs = vec![full_rec(WK, 0.0, 34.0, end), full_rec(WK + BUCKET_MS, 0.0, 23.0, end)];
        assert!(seven_day_resets(&recs).is_empty());
    }

    #[test]
    fn a_drop_into_the_floor_from_close_above_it_is_not_a_reset() {
        let end = WK + WEEK_MS;
        let recs = vec![full_rec(WK, 0.0, 8.0, end), full_rec(WK + DAY_MS, 0.0, 0.0, end)];
        assert!(seven_day_resets(&recs).is_empty());
    }

    #[test]
    fn a_scheduled_reset_seen_by_both_signals_is_one_reset() {
        let end = WK + DAY_MS;
        let recs = vec![
            full_rec(WK, 0.0, 80.0, end),
            UsageHistoryRecord { seven_day_resets_at: None, ..full_rec(end + 500, 0.0, 0.0, 0) },
            full_rec(end + 20 * 60 * 1000, 0.0, 0.0, end + WEEK_MS),
        ];
        assert_eq!(seven_day_resets(&recs), vec![end]);
    }

    #[test]
    fn a_stale_window_and_the_current_one_are_not_resets() {
        let end = WK + DAY_MS;
        let recs = vec![
            full_rec(WK, 0.0, 50.0, end),
            full_rec(end + 1_000, 0.0, 0.0, end + WEEK_MS),
            // A peer still on the old window, past the hour that would merge it.
            full_rec(end + 2 * RESET_JUMP_MS, 0.0, 50.0, end),
            full_rec(end + 2 * RESET_JUMP_MS + 1_000, 0.0, 1.0, end + WEEK_MS),
        ];
        assert_eq!(seven_day_resets(&recs), vec![end]);
    }

    #[test]
    fn a_poll_still_reporting_the_spent_window_does_not_colour_the_next_bucket() {
        let reset = WK + 3 * BUCKET_MS;
        let recs = vec![
            full_rec(reset - BUCKET_MS + 1_000, 0.0, 100.0, reset - 500),
            full_rec(reset + 53, 0.0, 100.0, reset + 367),
            full_rec(reset + 6_000, 0.0, 0.0, reset + WEEK_MS),
        ];
        let out = build_bucket_limit_pct(&recs, WK);
        assert_eq!(&out[2..4], &[Some(100.0), Some(0.0)]);
    }
}
