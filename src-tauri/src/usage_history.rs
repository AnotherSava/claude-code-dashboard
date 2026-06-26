use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One sample of the Anthropic usage-limits poll, flattened for easy
/// time-series processing (each JSONL line is effectively a named-column
/// CSV row). Percentages are stored raw as the API returns them (0..100,
/// unclamped) — graphing code decides how to normalize.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
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
    /// consecutive-delta walk in `build_week_chart`. A missing file is an empty
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

/// One bar of the work-intensity chart: a fixed 10-minute slot.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct WeekBucket {
    /// Percent of the 5h limit consumed in this 10-min slot (>= 0). The rate of
    /// consumption stands in for work intensity.
    pub intensity: f32,
    /// Whether any observation covered this slot. `false` = a gap (app closed /
    /// poller stopped), which renders distinctly from genuine idle
    /// (`has_data: true, intensity: 0`).
    pub has_data: bool,
}

/// Per-day roll-up shown to the right of each day row.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct DaySummary {
    /// Minutes Claude was active this day: 10-min buckets with intensity > 0.
    pub active_minutes: i64,
    /// Percent of the 7-day (weekly) quota consumed this day — the summed
    /// positive increments of `seven_day_pct`. A weekly-window reset shows as a
    /// drop, which clamps to 0, so a reset day still totals only the real
    /// consumption on either side of it (never a negative).
    pub weekly_pct: f32,
}

/// A week of work-intensity buckets plus the metadata the UI needs to label the
/// range and gate prev/next navigation.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WeekChart {
    pub week_start_ms: i64,
    pub week_end_ms: i64,
    /// Exactly `BUCKETS_PER_WEEK` entries, bucket `i` covering
    /// `[week_start_ms + i*BUCKET_MS, +BUCKET_MS)`.
    pub buckets: Vec<WeekBucket>,
    /// One entry per day (Mon..Sun), index `d` covering
    /// `[week_start_ms + d*DAY_MS, +DAY_MS)`.
    pub days: Vec<DaySummary>,
    /// Earliest / latest `ts` across the whole history, so the UI can disable
    /// "prev" once the displayed week reaches the oldest data.
    pub data_min_ms: Option<i64>,
    pub data_max_ms: Option<i64>,
    /// The reference "sustainable pace" marker (`FULL_INTENSITY_PCT`).
    pub full_intensity: f32,
}

pub const BUCKET_MS: i64 = 10 * 60 * 1000;
pub const BUCKETS_PER_DAY: usize = 6 * 24; // 144
pub const BUCKETS_PER_WEEK: usize = BUCKETS_PER_DAY * 7; // 1008
pub const DAY_MS: i64 = BUCKET_MS * BUCKETS_PER_DAY as i64;
pub const WEEK_MS: i64 = BUCKET_MS * BUCKETS_PER_WEEK as i64;
/// Consuming the whole 5h limit in 5 hours of sustained work spends exactly
/// `100 / 5 / 6` percent per 10-min bucket. Drawn as the chart's reference line.
pub const FULL_INTENSITY_PCT: f32 = 100.0 / 5.0 / 6.0;
/// An inter-observation gap longer than this (30 min, ~3 normal poll steps)
/// marks the spanned interior as no-data rather than attributing a single huge
/// delta across hours the app wasn't even running.
pub const GAP_THRESHOLD_MS: i64 = 3 * BUCKET_MS;

/// Lay `records` (assumed sorted ascending by `ts`) onto the fixed 1008-bucket
/// grid that starts at `week_start_ms`. Pure: no clock, no timezone — all
/// tz/DST/week-alignment logic lives in the calling command, so this is fully
/// unit-testable with synthetic records.
pub fn build_week_chart(records: &[UsageHistoryRecord], week_start_ms: i64) -> WeekChart {
    let week_end_ms = week_start_ms + WEEK_MS;
    let mut buckets = vec![WeekBucket { intensity: 0.0, has_data: false }; BUCKETS_PER_WEEK];
    let data_min_ms = records.first().map(|r| r.ts);
    let data_max_ms = records.last().map(|r| r.ts);

    for pair in records.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let (Some(prev_pct), Some(cur_pct)) = (prev.five_hour_pct, cur.five_hour_pct) else {
            continue; // can't compute a delta — leave the span as no-data
        };
        let dt = cur.ts - prev.ts;
        if dt <= 0 {
            continue; // duplicate sample or clock step back
        }

        // Intensity is the positive increment of the cumulative 5h counter.
        // We deliberately do NOT use `five_hour_resets_at` to detect a window
        // reset: for the fixed 5h window it jitters by ±1 min between polls
        // without any real reset, which would mis-attribute the absolute pct on
        // every step. A real reset instead shows as a large pct *drop*, which
        // clamps to 0 here — losing only the few percent accrued in the new
        // window's first poll (a tiny, ~once-per-5h undercount), never an
        // overcount.
        let delta = (cur_pct - prev_pct).max(0.0);

        let is_gap = dt > GAP_THRESHOLD_MS;

        // Clip the interval to the week before distributing it.
        let start = prev.ts.max(week_start_ms);
        let end = cur.ts.min(week_end_ms);
        if start >= end {
            continue;
        }

        // Time-weighted distribution: split the delta across the buckets the
        // interval overlaps, by overlap duration. A whole-interval-in-one-bucket
        // case gets the full delta; a straddling interval splits proportionally.
        let rate = delta / dt as f32; // percent per ms
        let first_idx = ((start - week_start_ms) / BUCKET_MS) as usize;
        let last_idx = ((end - 1 - week_start_ms) / BUCKET_MS) as usize;
        for idx in first_idx..=last_idx {
            let b_start = week_start_ms + idx as i64 * BUCKET_MS;
            let overlap = end.min(b_start + BUCKET_MS) - start.max(b_start);
            if overlap <= 0 {
                continue;
            }
            if is_gap {
                continue; // bucket stays no-data: app was closed across this span
            }
            buckets[idx].has_data = true;
            buckets[idx].intensity += rate * overlap as f32;
        }
    }

    // Per-day 7-day-quota consumption: the positive increments of seven_day_pct,
    // time-weighted across the days an interval spans. A weekly-window reset is a
    // pct drop that clamps to 0 (same robustness as the 5h path), so a reset day
    // still totals only the genuine consumption on either side of the reset.
    // Unlike the 5h intensity, gaps are NOT excluded: seven_day_pct is a slow
    // cumulative counter, so the rise across a closed-app span is real account
    // usage and is attributed to the day(s) it covers.
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

    let days = (0..7)
        .map(|d| {
            let row = &buckets[d * BUCKETS_PER_DAY..(d + 1) * BUCKETS_PER_DAY];
            let active = row.iter().filter(|b| b.has_data && b.intensity > 0.0).count();
            DaySummary { active_minutes: active as i64 * 10, weekly_pct: weekly[d] }
        })
        .collect();

    WeekChart { week_start_ms, week_end_ms, buckets, days, data_min_ms, data_max_ms, full_intensity: FULL_INTENSITY_PCT }
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

    // --- build_week_chart -------------------------------------------------

    /// Week start aligned to a bucket boundary (WEEK_MS is a multiple of
    /// BUCKET_MS), so `idx = (ts - WK) / BUCKET_MS` is exact in assertions.
    const WK: i64 = 100 * WEEK_MS;

    fn rec(ts: i64, pct: f32, resets: i64) -> UsageHistoryRecord {
        UsageHistoryRecord {
            ts,
            five_hour_pct: Some(pct),
            five_hour_resets_at: Some(resets),
            seven_day_pct: None,
            seven_day_resets_at: None,
        }
    }

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
    fn normal_accumulation_sums_deltas() {
        let r = 1;
        let recs = [
            rec(WK, 10.0, r),
            rec(WK + BUCKET_MS, 13.0, r),
            rec(WK + 2 * BUCKET_MS, 16.0, r),
        ];
        let chart = build_week_chart(&recs, WK);
        assert_eq!(chart.buckets.len(), BUCKETS_PER_WEEK);
        assert!(close(chart.buckets[0].intensity, 3.0) && chart.buckets[0].has_data);
        assert!(close(chart.buckets[1].intensity, 3.0) && chart.buckets[1].has_data);
        // No interval extends past the last observation, so the slot it starts
        // is untouched — idle vs no-data: this stays no-data.
        assert!(!chart.buckets[2].has_data && close(chart.buckets[2].intensity, 0.0));
        let total: f32 = chart.buckets.iter().map(|b| b.intensity).sum();
        assert!(close(total, 6.0));
        assert_eq!(chart.data_min_ms, Some(WK));
        assert_eq!(chart.data_max_ms, Some(WK + 2 * BUCKET_MS));
        assert!(close(chart.full_intensity, FULL_INTENSITY_PCT));
    }

    #[test]
    fn time_weighting_splits_across_boundary() {
        // A 15-min interval (Δ3) starting at a bucket boundary splits 2:1
        // across two 10-min buckets.
        let recs = [rec(WK, 10.0, 1), rec(WK + 15 * 60 * 1000, 13.0, 1)];
        let chart = build_week_chart(&recs, WK);
        assert!(close(chart.buckets[0].intensity, 2.0));
        assert!(close(chart.buckets[1].intensity, 1.0));
    }

    #[test]
    fn reset_drop_contributes_zero_never_negative() {
        // A window reset shows as a large pct drop (80 -> 5). The increment is
        // clamped to 0 (we don't trust the jittery resets_at as a reset signal),
        // never negative.
        let recs = [rec(WK, 80.0, 1), rec(WK + BUCKET_MS, 5.0, 2)];
        let chart = build_week_chart(&recs, WK);
        assert!(close(chart.buckets[0].intensity, 0.0));
        assert!(chart.buckets.iter().all(|b| b.intensity >= 0.0));
    }

    #[test]
    fn resets_at_jitter_without_pct_drop_is_normal_accumulation() {
        // Real data: the fixed 5h window's resets_at wobbles ±1min between polls
        // while pct climbs. That must read as normal accumulation (the increment),
        // not a reset that would attribute the absolute pct.
        let recs = [rec(WK, 28.0, 1_000_060), rec(WK + BUCKET_MS, 31.0, 1_000_000)];
        let chart = build_week_chart(&recs, WK);
        assert!(close(chart.buckets[0].intensity, 3.0));
    }

    #[test]
    fn multi_hour_gap_interior_is_no_data() {
        let r = 1;
        let five_h = 5 * 3600 * 1000;
        let recs = [
            rec(WK, 10.0, r),
            rec(WK + five_h, 20.0, r),          // 5h gap -> interior no-data
            rec(WK + five_h + BUCKET_MS, 23.0, r), // next interval re-marks data
        ];
        let chart = build_week_chart(&recs, WK);
        // Interior of the gap stays no-data...
        assert!(!chart.buckets[0].has_data);
        assert!(!chart.buckets[15].has_data);
        assert!(!chart.buckets[29].has_data);
        // ...but the post-gap interval (buckets[30]) is reclaimed as data.
        assert!(chart.buckets[30].has_data);
        assert!(close(chart.buckets[30].intensity, 3.0));
    }

    #[test]
    fn empty_input_is_all_no_data() {
        let chart = build_week_chart(&[], WK);
        assert_eq!(chart.buckets.len(), BUCKETS_PER_WEEK);
        assert!(chart.buckets.iter().all(|b| !b.has_data && b.intensity == 0.0));
        assert_eq!(chart.data_min_ms, None);
        assert_eq!(chart.data_max_ms, None);
    }

    #[test]
    fn records_outside_week_contribute_nothing() {
        let recs = [rec(WK - 10 * BUCKET_MS, 10.0, 1), rec(WK - 9 * BUCKET_MS, 13.0, 1)];
        let chart = build_week_chart(&recs, WK);
        assert!(chart.buckets.iter().all(|b| !b.has_data));
    }

    #[test]
    fn interval_straddling_week_start_counts_only_in_week_fraction() {
        // Half a bucket before the week to half a bucket after: dt = 1 bucket,
        // Δ2, only the in-week half (1.0) lands in bucket 0.
        let half = BUCKET_MS / 2;
        let recs = [rec(WK - half, 10.0, 1), rec(WK + half, 12.0, 1)];
        let chart = build_week_chart(&recs, WK);
        assert!(close(chart.buckets[0].intensity, 1.0));
    }

    #[test]
    fn none_pct_between_valid_is_skipped() {
        let recs = [
            rec(WK, 10.0, 1),
            UsageHistoryRecord {
                ts: WK + BUCKET_MS,
                five_hour_pct: None,
                five_hour_resets_at: Some(1),
                seven_day_pct: None,
                seven_day_resets_at: None,
            },
            rec(WK + 2 * BUCKET_MS, 16.0, 1),
        ];
        let chart = build_week_chart(&recs, WK);
        // Both pairs touch the None record, so nothing is attributed.
        assert!(chart.buckets.iter().all(|b| !b.has_data));
    }

    #[test]
    fn day_summary_active_minutes_and_weekly_pct() {
        // Mon: 5h climbs over two 10-min buckets (20 min active); seven_day
        // climbs 20 -> 22 -> 23 = +3% of the weekly quota.
        let recs = [
            full_rec(WK, 10.0, 20.0, 100),
            full_rec(WK + BUCKET_MS, 13.0, 22.0, 100),
            full_rec(WK + 2 * BUCKET_MS, 16.0, 23.0, 100),
        ];
        let chart = build_week_chart(&recs, WK);
        assert_eq!(chart.days.len(), 7);
        assert_eq!(chart.days[0].active_minutes, 20);
        assert!(close(chart.days[0].weekly_pct, 3.0));
        assert_eq!(chart.days[1].active_minutes, 0);
        assert!(close(chart.days[1].weekly_pct, 0.0));
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
        let chart = build_week_chart(&recs, WK);
        assert!(chart.days[0].weekly_pct >= 0.0);
        assert!(close(chart.days[0].weekly_pct, 0.0));
    }

    #[test]
    fn weekly_pct_splits_across_midnight() {
        // An interval spanning Mon 23:50 -> Tue 00:00+ splits its seven_day
        // delta across the two days by overlap.
        let recs = [
            full_rec(WK + DAY_MS - 5 * 60 * 1000, 10.0, 10.0, 100), // 5 min before midnight
            full_rec(WK + DAY_MS + 5 * 60 * 1000, 11.0, 14.0, 100), // 5 min after
        ];
        let chart = build_week_chart(&recs, WK);
        assert!(close(chart.days[0].weekly_pct, 2.0));
        assert!(close(chart.days[1].weekly_pct, 2.0));
    }
}
