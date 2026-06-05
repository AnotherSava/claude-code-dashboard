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
}
