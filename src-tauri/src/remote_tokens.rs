//! Disk persistence for remote-device token records — the peer-side counterpart
//! of `token_history.rs`, one file per device under `remote_tokens/` in the app
//! data dir. Mirrors `remote_usage.rs` in shape, and differs from it in exactly
//! one place, which is the point of this module existing separately.
//!
//! **Acceptance is keyed on `seq`, never on `ts`.** [`crate::remote_usage`] can
//! gate on `ts > held_max` because a single sequential poller emits strictly
//! increasing timestamps, so "newer than I hold" and "not yet seen" mean the
//! same thing there. Token records break that in ordinary operation:
//!
//! - concurrent session tails are appended in scan order, not chronological order;
//! - a fan-out revision re-appends a record whose `ts` is older than the file's max;
//! - the one-time historical import writes weeks of records below the current max.
//!
//! Under a `ts` gate each of those is silently discarded — no branch, no counter,
//! no log — and the loss is invisible because a missing bucket looks exactly like
//! an idle one. `seq` is assigned by the *sending* device's append order, so it
//! is monotonic by construction in the transport dimension while leaving the
//! event dimension free.
//!
//! Why keep peers' records at all: the chart is meant to show account-wide work,
//! and transcripts are per-machine. Without this, a device that did most of the
//! week's work simply doesn't appear.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::token_history::TokenRecord;

/// One device's persisted token records. The device name is repeated inside the
/// file because the filename is sanitized and can't be reversed.
#[derive(Serialize, Deserialize, Default)]
struct DeviceTokens {
    device: String,
    /// Sorted ascending by `seq` (maintained on every merge).
    records: Vec<TokenRecord>,
}

pub struct RemoteTokenStore {
    dir: PathBuf,
    data: Mutex<HashMap<String, DeviceTokens>>,
}

/// Device names are hostnames from peers' configs — almost always already
/// filesystem-safe, but never trusted: anything outside `[A-Za-z0-9._-]`
/// becomes `_`.
fn sanitize_filename(device: &str) -> String {
    device.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

impl RemoteTokenStore {
    pub fn new(dir: PathBuf) -> Self {
        let mut data = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| serde_json::from_str::<DeviceTokens>(&c).map_err(|e| e.to_string())) {
                    Ok(mut dt) if !dt.device.is_empty() => {
                        dt.records.sort_by_key(|r| r.seq);
                        data.insert(dt.device.clone(), dt);
                    }
                    Ok(_) => tracing::warn!(path = %path.display(), "remote token file without device name skipped"),
                    Err(e) => tracing::warn!(%e, path = %path.display(), "failed to read remote tokens"),
                }
            }
        }
        tracing::debug!(devices = data.len(), "remote tokens loaded");
        Self { dir, data: Mutex::new(data) }
    }

    /// Store the incoming records this device hasn't seen from `device`, keyed on
    /// the sender's `seq`. Returns how many were accepted.
    ///
    /// Idempotent: two pulls racing over an overlapping range can't duplicate,
    /// and a replayed range is a no-op. Even if a duplicate did slip through,
    /// `token_history::reduce_by_id` collapses it at read time — the `seq` gate
    /// is an efficiency measure, and correctness does not rest on it alone.
    pub fn merge_device(&self, device: &str, incoming: &[TokenRecord]) -> usize {
        if incoming.is_empty() {
            return 0;
        }
        let mut data = self.data.lock().unwrap();
        let dt = data.entry(device.to_string()).or_default();
        dt.device = device.to_string();
        let held_seq = dt.records.last().map(|r| r.seq);
        let mut accepted = 0;
        for r in incoming {
            if held_seq.is_none_or(|held| r.seq > held) {
                dt.records.push(r.clone());
                accepted += 1;
            }
        }
        if accepted == 0 {
            return 0; // nothing new — no disk write
        }
        dt.records.sort_by_key(|r| r.seq);

        let path = self.dir.join(format!("{}.json", sanitize_filename(device)));
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(?e, dir = %self.dir.display(), "failed to create remote tokens dir");
            return accepted;
        }
        match serde_json::to_string(&*dt) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(?e, path = %path.display(), "failed to write remote tokens");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize remote tokens"),
        }
        accepted
    }

    /// Every device's records, flattened — for the union in
    /// `commands::merged_token_records`, which reduces the combined set.
    pub fn all_records(&self) -> Vec<TokenRecord> {
        self.data.lock().unwrap().values().flat_map(|dt| dt.records.iter().cloned()).collect()
    }

    /// Highest `seq` held for `device`, or `0` when none — the `since` the sync
    /// receiver pulls from. Derived from the stored records rather than tracked
    /// separately, so it cannot drift from what is actually held.
    pub fn newest_seq(&self, device: &str) -> u64 {
        self.data.lock().unwrap().get(device).and_then(|dt| dt.records.last().map(|r| r.seq)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64, ts: i64, id: &str) -> TokenRecord {
        TokenRecord { ts, id: id.to_string(), seq, input: 10, cache_creation: 0, cache_read: 0, output: 5 }
    }

    fn temp_store() -> (RemoteTokenStore, PathBuf) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("remote_tokens_test_{}_{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        (RemoteTokenStore::new(dir.clone()), dir)
    }

    #[test]
    fn accepts_records_above_the_held_sequence() {
        let (store, dir) = temp_store();
        assert_eq!(store.merge_device("chrome", &[rec(1, 100, "a"), rec(2, 200, "b")]), 2);
        assert_eq!(store.newest_seq("chrome"), 2);
        assert_eq!(store.merge_device("chrome", &[rec(3, 300, "c")]), 1);
        assert_eq!(store.all_records().len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replayed_range_is_a_no_op() {
        let (store, dir) = temp_store();
        store.merge_device("chrome", &[rec(1, 100, "a"), rec(2, 200, "b")]);
        assert_eq!(store.merge_device("chrome", &[rec(1, 100, "a"), rec(2, 200, "b")]), 0);
        assert_eq!(store.all_records().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_backfilled_record_below_the_held_timestamp_still_transfers() {
        // The whole reason this store gates on seq and not ts. The historical
        // import appends weeks-old records with fresh sequence numbers; a ts
        // gate would drop every one of them without a trace.
        let (store, dir) = temp_store();
        store.merge_device("chrome", &[rec(1, 9_000_000, "recent")]);
        assert_eq!(store.merge_device("chrome", &[rec(2, 1_000, "ancient")]), 1);
        assert!(store.all_records().iter().any(|r| r.id == "ancient"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn devices_are_kept_apart() {
        let (store, dir) = temp_store();
        store.merge_device("chrome", &[rec(1, 100, "a"), rec(2, 200, "b")]);
        store.merge_device("air", &[rec(1, 150, "c")]);
        assert_eq!(store.newest_seq("chrome"), 2);
        assert_eq!(store.newest_seq("air"), 1);
        assert_eq!(store.all_records().len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_survive_a_reload_from_disk() {
        let (store, dir) = temp_store();
        store.merge_device("chrome", &[rec(1, 100, "a"), rec(2, 200, "b")]);
        let reopened = RemoteTokenStore::new(dir.clone());
        assert_eq!(reopened.newest_seq("chrome"), 2);
        assert_eq!(reopened.all_records().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unknown_device_has_no_sequence_yet() {
        let (store, dir) = temp_store();
        assert_eq!(store.newest_seq("nobody"), 0);
        assert!(store.all_records().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
