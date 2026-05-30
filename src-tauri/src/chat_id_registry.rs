use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Maps a Claude Code `session_id` to the `chat_id` (dashboard row) it was
/// first seen under. A session's cwd can change mid-conversation (the agent
/// `cd`s into a subdirectory), which would otherwise fragment one conversation
/// across multiple rows since `chat_id` is cwd-derived. Locking to the first
/// cwd-derived id keeps a session on one row. `/clear` mints a new session_id
/// with the same cwd, so it re-derives the same id and continuity holds.
pub struct ChatIdRegistry {
    path: PathBuf,
    data: Mutex<HashMap<String, String>>,
}

impl ChatIdRegistry {
    pub fn new(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(?e, path = %path.display(), "failed to read chat id registry");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        tracing::debug!(sessions = data.len(), "chat id registry loaded");
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    /// Returns the stable chat_id for `session_id`. On first sight, locks in
    /// `derived` and persists; on later calls, returns the locked id
    /// regardless of `derived` (absorbs mid-session cwd changes). An empty
    /// `session_id` can't be locked — `derived` is returned as-is.
    pub fn resolve(&self, session_id: &str, derived: &str) -> String {
        if session_id.is_empty() {
            return derived.to_string();
        }
        let mut data = self.data.lock().unwrap();
        if let Some(existing) = data.get(session_id) {
            return existing.clone();
        }
        data.insert(session_id.to_string(), derived.to_string());
        drop(data);
        self.save_to_disk();
        derived.to_string()
    }

    /// Drops the mapping for a session that has ended (`SessionEnd`).
    pub fn forget(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let removed = self.data.lock().unwrap().remove(session_id).is_some();
        if removed {
            self.save_to_disk();
        }
    }

    fn save_to_disk(&self) {
        let data = self.data.lock().unwrap();
        match serde_json::to_string_pretty(&*data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write chat id registry");
                }
            }
            Err(e) => {
                tracing::warn!(?e, "failed to serialize chat id registry");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ChatIdRegistry {
        // Unique path per call: `cargo test` runs these in parallel and they all
        // share one process id, so a single pid-keyed path would let one test's
        // save_to_disk land between another's remove_file and new() (which reads
        // the file) — a flaky cross-test race. The counter isolates each call;
        // remove_file still guards against a stale file from a reused pid across
        // separate test runs.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "chat_id_registry_test_{}_{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        ChatIdRegistry::new(path)
    }

    #[test]
    fn unseen_session_returns_and_stores_derived() {
        let r = registry();
        assert_eq!(r.resolve("s1", "assistant"), "assistant");
        assert_eq!(r.data.lock().unwrap().get("s1").unwrap(), "assistant");
    }

    #[test]
    fn seen_session_keeps_first_id_despite_new_derived() {
        let r = registry();
        r.resolve("s1", "assistant");
        assert_eq!(r.resolve("s1", "data"), "assistant", "cwd changed but row is stable");
    }

    #[test]
    fn empty_session_returns_derived_without_storing() {
        let r = registry();
        assert_eq!(r.resolve("", "data"), "data");
        assert!(r.data.lock().unwrap().is_empty());
    }

    #[test]
    fn forget_drops_mapping() {
        let r = registry();
        r.resolve("s1", "assistant");
        r.forget("s1");
        assert!(r.data.lock().unwrap().get("s1").is_none());
        assert_eq!(r.resolve("s1", "data"), "data", "re-derives after forget");
    }
}
