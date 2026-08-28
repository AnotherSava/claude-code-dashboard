//! Append-only record of Claude API token usage, read from Claude Code's own
//! transcripts by [`crate::token_scan`].
//!
//! Exists because the usage-limits percentages in [`crate::usage_history`] are
//! denominated in the *plan's* quota: the same work reports a different
//! percentage after a subscription tier change, and only percentages were ever
//! stored, so past absolute work is unrecoverable. Tokens are plan-independent,
//! so this series stays comparable across tier changes and promotions.
//!
//! Two invariants a future change must not quietly break:
//!
//! - **Components are stored separately, never pre-weighted.** Which components
//!   count as "work" is a display decision (see `WORK_COMPONENTS` below) and has
//!   already changed once. Collapsing them at write time over a source that
//!   self-deletes in ~30 days would repeat the mistake that made the percentage
//!   history unrecoverable.
//! - **`seq` is the transport clock, `ts` is the event clock.** Records do not
//!   arrive in `ts` order — concurrent session tails interleave, a fan-out
//!   revision re-appends an older `ts`, and the historical import writes weeks
//!   below the current maximum. Anything that gates on "newer than what I hold"
//!   must use `seq`; see [`crate::remote_tokens`].

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One Claude API response's token usage, keyed by its `message.id`.
///
/// Claude Code writes one transcript line per *content block* of a response,
/// each repeating the whole `usage` object, so the same `id` recurs — 54.98% of
/// lines in a real 660-file tree. Summing lines inflates totals 2.03x, hence
/// [`reduce_by_id`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenRecord {
    /// Response timestamp, ms since epoch. The bucket this record falls into.
    pub ts: i64,
    /// `message.id` — the deduplication key.
    pub id: String,
    /// Per-device append order, assigned by [`TokenHistoryStore::append_batch`].
    /// Monotonic by construction; unrelated to `ts`. Starts at 1, so 0 is an
    /// unambiguous "nothing held yet" sentinel for a sync watermark — with a
    /// 0-based sequence, a peer asking for everything (`since = 0`) would never
    /// be sent the very first record.
    pub seq: u64,
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl TokenRecord {
    /// The tokens this record contributes to the chart: input + cache creation +
    /// output.
    ///
    /// `cache_read` is deliberately excluded. It is 97.1% of the raw sum and
    /// tracks how long a conversation has grown rather than how much work was
    /// done, so including it swamps the signal: against the account's own 5h
    /// counter over 621 shared 10-minute buckets, this combination correlates at
    /// rho = 0.799 where all four components together manage 0.473 and
    /// `cache_read` alone 0.444. It is still stored, so changing this is a view
    /// change rather than a re-import.
    pub fn work_tokens(&self) -> u64 {
        self.input + self.cache_creation + self.output
    }
}

/// Collapse repeated `message.id`s, taking the maximum of each component.
///
/// Claude Code writes `output_tokens` incrementally across a response's content
/// blocks, and the last line carries the final value — verified as `last == max`
/// across all 21,382 fan-out groups in a real tree, so the maximum is the true
/// total rather than merely a safe guess.
///
/// Max-per-field is idempotent, commutative and associative. That is what makes
/// a re-scan of an already-consumed file, a late-arriving fan-out line, an
/// out-of-order sync pull and an overlapping range fetch all harmless *by
/// construction* rather than by remembering to avoid them.
///
/// The `ts` kept is the earliest seen for that id (a response belongs to the
/// moment it started), and `seq` the highest, so a reduced record still sorts
/// correctly in the transport dimension. Output is sorted by `ts`.
pub fn reduce_by_id(records: &[TokenRecord]) -> Vec<TokenRecord> {
    let mut by_id: HashMap<&str, TokenRecord> = HashMap::with_capacity(records.len());
    for r in records {
        by_id
            .entry(r.id.as_str())
            .and_modify(|acc| {
                acc.ts = acc.ts.min(r.ts);
                acc.seq = acc.seq.max(r.seq);
                acc.input = acc.input.max(r.input);
                acc.cache_creation = acc.cache_creation.max(r.cache_creation);
                acc.cache_read = acc.cache_read.max(r.cache_read);
                acc.output = acc.output.max(r.output);
            })
            .or_insert_with(|| r.clone());
    }
    let mut out: Vec<TokenRecord> = by_id.into_values().collect();
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Append-only JSONL sink for token records (`token_history.jsonl` in the app
/// data dir). Mirrors [`crate::usage_history::UsageHistoryStore`], including
/// living outside config.json so it survives the deploy step that overwrites it.
pub struct TokenHistoryStore {
    path: PathBuf,
    // Serializes appends and guards `next_seq`, so a sequence number is never
    // handed out twice even though the scanner may append from several passes.
    write_lock: Mutex<u64>,
}

impl TokenHistoryStore {
    /// Opens the store and recovers the next sequence number from disk, so
    /// numbering continues across restarts.
    pub fn new(path: PathBuf) -> Self {
        let store = Self { path, write_lock: Mutex::new(0) };
        let next = store.read_all().last().map(|r| r.seq + 1).unwrap_or(1);
        *store.write_lock.lock().unwrap() = next;
        store
    }

    /// Append `records`, assigning each a fresh `seq` in the order given.
    /// Returns the highest sequence number written, or `None` for an empty
    /// batch. Records are written verbatim otherwise — deduplication happens at
    /// read time, so a replayed batch costs disk but never corrupts a total.
    pub fn append_batch(&self, records: &[TokenRecord]) -> Option<u64> {
        if records.is_empty() {
            return None;
        }
        let mut next_seq = self.write_lock.lock().unwrap();
        let mut lines = String::new();
        let mut highest = None;
        for record in records {
            let stamped = TokenRecord { seq: *next_seq, ..record.clone() };
            match serde_json::to_string(&stamped) {
                Ok(json) => {
                    lines.push_str(&json);
                    lines.push('\n');
                    highest = Some(*next_seq);
                    *next_seq += 1;
                }
                Err(e) => tracing::warn!(?e, id = %record.id, "failed to serialize token record"),
            }
        }
        if lines.is_empty() {
            return None;
        }
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(lines.as_bytes()));
        if let Err(e) = result {
            tracing::warn!(?e, path = %self.path.display(), "failed to append token history");
            return None;
        }
        highest
    }

    /// Every stored record, sorted by `seq`. Malformed lines are dropped; a
    /// missing file is an empty history, not an error. Callers that need totals
    /// must pass the result through [`reduce_by_id`] first.
    pub fn read_all(&self) -> Vec<TokenRecord> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!(?e, path = %self.path.display(), "failed to read token history");
                return Vec::new();
            }
        };
        let mut records: Vec<TokenRecord> = contents.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
        records.sort_by_key(|r| r.seq);
        records
    }

    /// The highest `seq` on disk — this device's sync tip. `None` when empty.
    pub fn newest_seq(&self) -> Option<u64> {
        self.read_all().last().map(|r| r.seq)
    }

    /// Records with `seq > since`, oldest first, capped at `limit`. The range a
    /// peer asks for; the cap bounds one response, and the peer's next push
    /// advertises the same tip again so a truncated range is simply re-asked.
    pub fn records_since_seq(&self, since: u64, limit: usize) -> Vec<TokenRecord> {
        self.read_all().into_iter().filter(|r| r.seq > since).take(limit).collect()
    }
}

/// One bar of the token intensity chart: the same fixed 10-minute slot the
/// percentage chart uses.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct TokenBucket {
    /// Work tokens attributed to this slot.
    pub tokens: f64,
    /// Whether this slot is inside the range we have token data for. `false`
    /// renders as unknown rather than idle.
    pub has_data: bool,
}

/// Per-day roll-up shown to the right of each day row.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct TokenDaySummary {
    /// Minutes Claude was active this day: 10-min buckets carrying any tokens.
    pub active_minutes: i64,
    pub tokens: f64,
}

/// A week of token buckets. Mirrors [`crate::usage_history::WeekChart`] but
/// carries an axis maximum instead of a "sustainable pace" reference: tokens
/// have no quota to be a fraction of, so there is no honest pace line — see
/// `axis_max_tokens`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TokenWeekChart {
    pub week_start_ms: i64,
    pub week_end_ms: i64,
    pub buckets: Vec<TokenBucket>,
    pub days: Vec<TokenDaySummary>,
    pub data_min_ms: Option<i64>,
    pub data_max_ms: Option<i64>,
    /// Full-height value for a bar, so the frontend needs no quota knowledge.
    /// A *stated* ceiling rather than a reference line: a rolling median of the
    /// user's own history would drift 2.1x across four weeks with no change in
    /// behaviour, so identical work would cross it purely from baseline movement
    /// while reading as an absolute marker.
    pub axis_max_tokens: f64,
}

/// Default full-height value for a 10-minute bucket. Clips 4.5% of active
/// buckets against a measured p50 of 77k, p90 of 666k and p99 of 1517k — about
/// the same share the percentage chart clips at its 2x-pace threshold. Halving
/// it to 500k was tried and reverted: it clipped 15.6% across all history and
/// 25.8% of a recent busy week, flattening the top quarter of a working day.
/// Overridable via `config.intensity_axis_max_tokens`.
pub const DEFAULT_AXIS_MAX_TOKENS: f64 = 1_000_000.0;

/// Lay `records` onto the fixed 1008-bucket grid starting at `week_start_ms`.
///
/// Unlike the percentage chart this is a plain histogram: each record is a point
/// event adding its [`TokenRecord::work_tokens`] to the bucket containing its
/// `ts`. There are no interval deltas, so there is no reset to clamp, no rate to
/// time-weight and no inter-observation gap to exclude.
///
/// `has_data` is simply "inside the range we hold token data for". A bucket
/// there with no records is genuine idle: transcripts are written by Claude Code
/// itself, so unlike the usage poller, the dashboard being closed loses nothing
/// — the scanner catches up afterwards. The one thing this cannot see is a
/// *device* whose records have not reached this machine; those buckets read as
/// idle until sync delivers them, which is why the range is taken from the
/// merged record set rather than from local data alone.
///
/// Pure: no clock, no timezone. Callers pass an already-reduced record set.
pub fn build_token_week_chart(records: &[TokenRecord], week_start_ms: i64, axis_max_tokens: f64) -> TokenWeekChart {
    use crate::usage_history::{BUCKETS_PER_DAY, BUCKETS_PER_WEEK, BUCKET_MS, WEEK_MS};

    let week_end_ms = week_start_ms + WEEK_MS;
    let mut buckets = vec![TokenBucket { tokens: 0.0, has_data: false }; BUCKETS_PER_WEEK];
    let data_min_ms = records.iter().map(|r| r.ts).min();
    let data_max_ms = records.iter().map(|r| r.ts).max();

    // Mark the covered span first, so a slot inside the token era with no work
    // reads as idle while everything outside it stays unknown.
    if let (Some(min), Some(max)) = (data_min_ms, data_max_ms) {
        for (idx, bucket) in buckets.iter_mut().enumerate() {
            let b_start = week_start_ms + idx as i64 * BUCKET_MS;
            bucket.has_data = b_start + BUCKET_MS > min && b_start <= max;
        }
    }

    for r in records {
        if r.ts < week_start_ms || r.ts >= week_end_ms {
            continue;
        }
        let idx = ((r.ts - week_start_ms) / BUCKET_MS) as usize;
        buckets[idx].tokens += r.work_tokens() as f64;
        buckets[idx].has_data = true;
    }

    let days = (0..7)
        .map(|d| {
            let row = &buckets[d * BUCKETS_PER_DAY..(d + 1) * BUCKETS_PER_DAY];
            let active = row.iter().filter(|b| b.tokens > 0.0).count();
            TokenDaySummary { active_minutes: active as i64 * 10, tokens: row.iter().map(|b| b.tokens).sum() }
        })
        .collect();

    TokenWeekChart { week_start_ms, week_end_ms, buckets, days, data_min_ms, data_max_ms, axis_max_tokens }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_history::{BUCKETS_PER_WEEK, BUCKET_MS, DAY_MS};

    fn record(id: &str, ts: i64, input: u64, cache_creation: u64, cache_read: u64, output: u64) -> TokenRecord {
        TokenRecord { ts, id: id.to_string(), seq: 0, input, cache_creation, cache_read, output }
    }

    #[test]
    fn work_tokens_excludes_cache_read() {
        // cache_read is 97% of the raw sum and tracks conversation length, so it
        // must not reach the chart even though it is stored.
        let r = record("a", 0, 10, 20, 9_000, 30);
        assert_eq!(r.work_tokens(), 60);
    }

    #[test]
    fn reduce_takes_the_max_of_each_fan_out_line() {
        // One response, three content blocks: output grows, the rest repeat.
        let lines = vec![
            record("m1", 100, 5, 40, 900, 10),
            record("m1", 100, 5, 40, 900, 60),
            record("m1", 100, 5, 40, 900, 120),
        ];
        let reduced = reduce_by_id(&lines);
        assert_eq!(reduced.len(), 1);
        assert_eq!(reduced[0].output, 120);
        assert_eq!(reduced[0].work_tokens(), 5 + 40 + 120);
    }

    #[test]
    fn reduce_is_idempotent_so_a_rescan_cannot_inflate_totals() {
        // The property the whole design leans on: re-reading a file already
        // consumed, or pulling an overlapping sync range, must be a no-op.
        let lines = vec![record("m1", 100, 5, 40, 900, 120), record("m2", 200, 7, 0, 100, 9)];
        let once = reduce_by_id(&lines);
        let replayed: Vec<TokenRecord> = lines.iter().chain(lines.iter()).cloned().collect();
        assert_eq!(reduce_by_id(&replayed), once);
    }

    #[test]
    fn reduce_is_order_independent() {
        // Sync delivers records out of order; the reduction must not care.
        let a = record("m1", 100, 5, 40, 900, 10);
        let b = record("m1", 100, 5, 40, 900, 120);
        assert_eq!(reduce_by_id(&[a.clone(), b.clone()]), reduce_by_id(&[b, a]));
    }

    #[test]
    fn reduce_keeps_the_earliest_ts_and_highest_seq() {
        let mut early = record("m1", 100, 1, 0, 0, 1);
        early.seq = 3;
        let mut late = record("m1", 500, 1, 0, 0, 2);
        late.seq = 9;
        let reduced = reduce_by_id(&[late, early]);
        assert_eq!(reduced[0].ts, 100);
        assert_eq!(reduced[0].seq, 9);
    }

    #[test]
    fn reduce_sorts_by_ts() {
        let reduced = reduce_by_id(&[record("b", 300, 1, 0, 0, 0), record("a", 100, 1, 0, 0, 0)]);
        assert_eq!(reduced.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    fn temp_store() -> (TokenHistoryStore, std::path::PathBuf) {
        let mut path = std::env::temp_dir();
        path.push(format!("token_history_test_{}_{:?}.jsonl", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_file(&path);
        (TokenHistoryStore::new(path.clone()), path)
    }

    #[test]
    fn append_assigns_consecutive_sequence_numbers_from_one() {
        // Numbering starts at 1 so that 0 can mean "nothing held" on the wire.
        let (store, path) = temp_store();
        assert_eq!(store.append_batch(&[record("a", 1, 1, 0, 0, 1), record("b", 2, 1, 0, 0, 1)]), Some(2));
        assert_eq!(store.append_batch(&[record("c", 3, 1, 0, 0, 1)]), Some(3));
        assert_eq!(store.read_all().iter().map(|r| r.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sequence_continues_across_a_restart() {
        // Numbering is recovered from disk, so a peer's watermark stays valid.
        let (store, path) = temp_store();
        store.append_batch(&[record("a", 1, 1, 0, 0, 1), record("b", 2, 1, 0, 0, 1)]);
        let reopened = TokenHistoryStore::new(path.clone());
        assert_eq!(reopened.append_batch(&[record("c", 3, 1, 0, 0, 1)]), Some(3));
        assert_eq!(reopened.newest_seq(), Some(3));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_peer_asking_for_everything_gets_the_very_first_record() {
        // The reason sequences start at 1: `since = 0` means "I hold nothing",
        // and must not silently exclude the oldest record.
        let (store, path) = temp_store();
        store.append_batch(&[record("a", 1, 1, 0, 0, 1), record("b", 2, 1, 0, 0, 1), record("c", 3, 1, 0, 0, 1)]);
        let all = store.records_since_seq(0, 10);
        assert_eq!(all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        let tail = store.records_since_seq(1, 10);
        assert_eq!(tail.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["b", "c"]);
        assert_eq!(store.records_since_seq(0, 1).len(), 1, "limit caps one response");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_batch_and_missing_file_are_not_errors() {
        let (store, path) = temp_store();
        assert_eq!(store.append_batch(&[]), None);
        assert!(store.read_all().is_empty());
        assert_eq!(store.newest_seq(), None);
        let _ = std::fs::remove_file(path);
    }

    const WEEK: i64 = 1_000_000_000_000;

    fn chart(records: &[TokenRecord]) -> TokenWeekChart {
        build_token_week_chart(records, WEEK, DEFAULT_AXIS_MAX_TOKENS)
    }

    #[test]
    fn a_record_lands_in_the_bucket_containing_its_timestamp() {
        let c = chart(&[record("a", WEEK + BUCKET_MS + 60_000, 5, 40, 900, 120)]);
        assert_eq!(c.buckets[1].tokens, 165.0);
        assert_eq!(c.buckets[0].tokens, 0.0);
    }

    #[test]
    fn records_in_one_bucket_sum() {
        // A histogram, not a rate: two responses in the same 10 minutes add.
        let c = chart(&[record("a", WEEK + 1, 10, 0, 0, 1), record("b", WEEK + 2, 20, 0, 0, 2)]);
        assert_eq!(c.buckets[0].tokens, 33.0);
    }

    #[test]
    fn records_outside_the_week_are_ignored() {
        let c = chart(&[record("before", WEEK - 1, 99, 0, 0, 0), record("after", WEEK + BUCKETS_PER_WEEK as i64 * BUCKET_MS, 99, 0, 0, 0)]);
        assert!(c.buckets.iter().all(|b| b.tokens == 0.0));
    }

    #[test]
    fn coverage_marks_only_the_span_we_hold_data_for() {
        // Outside the token era is unknown, not idle — nothing was recorded that
        // far back, so claiming "no work" there would be a lie.
        let c = chart(&[record("a", WEEK + 10 * BUCKET_MS, 1, 0, 0, 0), record("b", WEEK + 20 * BUCKET_MS, 1, 0, 0, 0)]);
        assert!(!c.buckets[9].has_data, "before the first record is unknown");
        assert!(c.buckets[10].has_data);
        assert!(c.buckets[15].has_data, "a quiet slot inside the era is genuine idle");
        assert_eq!(c.buckets[15].tokens, 0.0);
        assert!(c.buckets[20].has_data);
        assert!(!c.buckets[21].has_data, "after the last record is unknown");
    }

    #[test]
    fn an_empty_history_is_entirely_unknown() {
        let c = chart(&[]);
        assert!(c.buckets.iter().all(|b| !b.has_data && b.tokens == 0.0));
        assert_eq!(c.data_min_ms, None);
    }

    #[test]
    fn day_summary_totals_tokens_and_active_minutes() {
        let day2 = WEEK + DAY_MS;
        let c = chart(&[
            record("a", WEEK + 1, 100, 0, 0, 0),
            record("b", WEEK + BUCKET_MS + 1, 200, 0, 0, 0),
            record("c", day2 + 1, 50, 0, 0, 0),
        ]);
        assert_eq!(c.days[0].tokens, 300.0);
        assert_eq!(c.days[0].active_minutes, 20, "two distinct 10-min buckets");
        assert_eq!(c.days[1].tokens, 50.0);
        assert_eq!(c.days[1].active_minutes, 10);
        assert_eq!(c.days[2].active_minutes, 0);
    }

    #[test]
    fn cache_read_never_reaches_the_chart() {
        // The unit decision, pinned: a huge cache_read must not inflate a bar.
        let c = chart(&[record("a", WEEK + 1, 0, 0, 9_000_000, 10)]);
        assert_eq!(c.buckets[0].tokens, 10.0);
    }

    #[test]
    fn axis_max_is_carried_through_for_the_frontend() {
        assert_eq!(build_token_week_chart(&[], WEEK, 250_000.0).axis_max_tokens, 250_000.0);
    }
}
