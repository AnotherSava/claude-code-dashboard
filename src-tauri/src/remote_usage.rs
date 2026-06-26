//! Disk persistence for remote-device usage-limit samples — the peer-side
//! counterpart of `usage_history.rs`, one file per device under `remote_usage/`
//! in the app data dir. Each device pushes its `usage_history` records (the
//! Anthropic 5h/7d polls) to its peers; a receiver stores them here, keyed by
//! the sender's device name, kept separate from its own local history.
//!
//! Why keep them at all: the 5h/7d usage counter is account-wide, so a peer's
//! polls during the windows *this* device's app was closed describe the same
//! timeline. `commands::merged_usage_records` unions every device's records
//! into the Work-intensity chart, so a gap on one device is filled by another's
//! coverage. Lives in its own dir (not config.json) so it survives the deploy
//! step that overwrites config.json — same rationale as `remote_history`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::usage_history::UsageHistoryRecord;

/// One device's persisted usage samples. The device name is repeated inside the
/// file because the filename is sanitized and can't be reversed.
#[derive(Serialize, Deserialize, Default)]
struct DeviceUsage {
    device: String,
    /// Sorted ascending by `ts` (maintained on every merge).
    records: Vec<UsageHistoryRecord>,
}

pub struct RemoteUsageStore {
    dir: PathBuf,
    data: Mutex<HashMap<String, DeviceUsage>>,
}

/// Device names are hostnames from peers' configs — almost always already
/// filesystem-safe, but never trusted: anything outside `[A-Za-z0-9._-]`
/// becomes `_`.
fn sanitize_filename(device: &str) -> String {
    device.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

impl RemoteUsageStore {
    pub fn new(dir: PathBuf) -> Self {
        let mut data = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| serde_json::from_str::<DeviceUsage>(&c).map_err(|e| e.to_string())) {
                    Ok(mut du) if !du.device.is_empty() => {
                        du.records.sort_by_key(|r| r.ts);
                        data.insert(du.device.clone(), du);
                    }
                    Ok(_) => tracing::warn!(path = %path.display(), "remote usage file without device name skipped"),
                    Err(e) => tracing::warn!(%e, path = %path.display(), "failed to read remote usage"),
                }
            }
        }
        tracing::debug!(devices = data.len(), "remote usage loaded");
        Self { dir, data: Mutex::new(data) }
    }

    /// Append the incoming records newer than what we already hold for `device`,
    /// keep the list sorted ascending, and persist. The `ts > held_max` filter
    /// is a cheap dedup against replays (a restarted sender re-sends from a zero
    /// watermark); since the counter is append-only this never drops real data.
    pub fn merge_device(&self, device: &str, incoming: &[UsageHistoryRecord]) {
        if incoming.is_empty() {
            return;
        }
        let mut data = self.data.lock().unwrap();
        let du = data.entry(device.to_string()).or_default();
        du.device = device.to_string();
        let held_max = du.records.last().map(|r| r.ts).unwrap_or(i64::MIN);
        let mut added = false;
        for r in incoming {
            if r.ts > held_max {
                du.records.push(r.clone());
                added = true;
            }
        }
        if !added {
            return; // nothing new — no disk write
        }
        du.records.sort_by_key(|r| r.ts);

        let path = self.dir.join(format!("{}.json", sanitize_filename(device)));
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(?e, dir = %self.dir.display(), "failed to create remote usage dir");
            return;
        }
        match serde_json::to_string(&*du) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(?e, path = %path.display(), "failed to write remote usage");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize remote usage"),
        }
    }

    /// Every device's records, flattened — for the chart-build union in
    /// `commands::merged_usage_records`. Caller re-sorts the combined set.
    pub fn all_records(&self) -> Vec<UsageHistoryRecord> {
        self.data.lock().unwrap().values().flat_map(|du| du.records.iter().cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: i64, pct: f32) -> UsageHistoryRecord {
        UsageHistoryRecord {
            ts,
            five_hour_pct: Some(pct),
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("claude_dashboard_remote_usage_{tag}_{}", std::process::id()))
    }

    #[test]
    fn round_trip_save_and_load() {
        let dir = temp_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteUsageStore::new(dir.clone());
        store.merge_device("laptop", &[rec(10, 5.0), rec(20, 8.0)]);

        let store2 = RemoteUsageStore::new(dir.clone());
        let records = store2.all_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].ts, 10);
        assert_eq!(records[1].five_hour_pct, Some(8.0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_dedups_by_ts_and_stays_sorted() {
        let dir = temp_dir("dedup");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteUsageStore::new(dir.clone());
        store.merge_device("laptop", &[rec(10, 5.0), rec(20, 8.0)]);
        // Replay (10, 20 already held) plus a genuinely newer 30.
        store.merge_device("laptop", &[rec(10, 5.0), rec(20, 8.0), rec(30, 11.0)]);

        let records = store.all_records();
        assert_eq!(records.len(), 3, "replayed entries dropped, only the new one added");
        assert_eq!(records.iter().map(|r| r.ts).collect::<Vec<_>>(), vec![10, 20, 30]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_merge_writes_nothing() {
        let dir = temp_dir("empty");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteUsageStore::new(dir.clone());
        store.merge_device("laptop", &[]);
        assert!(!dir.join("laptop.json").exists(), "no file for an empty merge");
        assert!(store.all_records().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn devices_get_separate_files_and_all_records_flattens() {
        let dir = temp_dir("files");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteUsageStore::new(dir.clone());
        store.merge_device("laptop.local", &[rec(10, 5.0)]);
        store.merge_device("desk:top", &[rec(20, 8.0)]);

        assert!(dir.join("laptop.local.json").exists());
        assert!(dir.join("desk_top.json").exists(), "unsafe chars sanitized");

        let store2 = RemoteUsageStore::new(dir.clone());
        let mut records = store2.all_records();
        records.sort_by_key(|r| r.ts);
        assert_eq!(records.len(), 2, "records from both devices flattened");
        assert_eq!(records[0].ts, 10);
        assert_eq!(records[1].ts, 20);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_loads_empty() {
        let store = RemoteUsageStore::new(temp_dir("missing_nonexistent"));
        assert!(store.all_records().is_empty());
    }
}
