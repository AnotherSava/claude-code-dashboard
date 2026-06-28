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
        let json = match serde_json::to_string_pretty(&*data) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(?e, "failed to serialize chat id registry");
                return;
            }
        };
        // Atomic write: write a sibling temp file, then rename it over the
        // target. rename is atomic on the same filesystem, so a crash or a
        // concurrent reader never sees the truncated file a plain `fs::write`
        // would leave mid-write — a torn file reloads as empty (`unwrap_or_default`)
        // and silently drops every session→chat_id lock. The mutex is held
        // through the rename so overlapping saves stay totally ordered; the last
        // to finish reflects the latest map.
        let tmp = self.path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, json.as_bytes()) {
            tracing::warn!(?e, path = %tmp.display(), "failed to write chat id registry");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            tracing::warn!(?e, path = %self.path.display(), "failed to persist chat id registry");
            let _ = std::fs::remove_file(&tmp);
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
    fn resolve_persists_across_reload_with_no_temp_left() {
        let r = registry();
        let path = r.path.clone();
        r.resolve("s1", "assistant");
        // A fresh registry on the same path sees the atomically-written mapping.
        let r2 = ChatIdRegistry::new(path.clone());
        assert_eq!(r2.resolve("s1", "other"), "assistant", "persisted lock survives reload");
        // The temp file is renamed away, never left behind.
        assert!(!path.with_extension("tmp").exists(), "no temp file leaked");
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
