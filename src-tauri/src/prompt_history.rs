use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::state::{AgentSession, PersistedSession};

pub struct PromptHistoryStore {
    path: PathBuf,
    data: Mutex<HashMap<String, PersistedSession>>,
}

impl PromptHistoryStore {
    pub fn new(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(?e, path = %path.display(), "failed to read prompt history");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        tracing::debug!(sessions = data.len(), "prompt history loaded");
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn get(&self, session_id: &str) -> Option<PersistedSession> {
        self.data.lock().unwrap().get(session_id).cloned()
    }

    /// True if any session has ever been persisted. Used by the onboarding
    /// flow as the signal that the dashboard has received at least one hook
    /// hit from Claude Code — once that has happened, the setup panel hides
    /// permanently across restarts.
    pub fn has_any_entries(&self) -> bool {
        !self.data.lock().unwrap().is_empty()
    }

    pub fn save_session(&self, session: &AgentSession) {
        let mut data = self.data.lock().unwrap();
        data.insert(
            session.id.clone(),
            PersistedSession {
                dialog: session.dialog.clone(),
                original_prompt: session.original_prompt.clone(),
                task_started_at: session.task_started_at,
                last_input_tokens: session.input_tokens,
            },
        );
    }

    pub fn remove(&self, session_id: &str) {
        self.data.lock().unwrap().remove(session_id);
    }

    pub fn save_to_disk(&self) {
        let data = self.data.lock().unwrap();
        match serde_json::to_string_pretty(&*data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write prompt history");
                }
            }
            Err(e) => {
                tracing::warn!(?e, "failed to serialize prompt history");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DialogEntry, DialogRole, Status};

    #[test]
    fn round_trip_save_and_load() {
        let dir = std::env::temp_dir().join(format!(
            "claude_dashboard_prompt_history_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompt_history.json");

        let store = PromptHistoryStore::new(path.clone());
        let entry = DialogEntry {
            role: DialogRole::User,
            text: "fix foo".into(),
            timestamp: 1000,
            status: Status::Working,
            task_start: true,
        };
        {
            let mut data = store.data.lock().unwrap();
            data.insert(
                "s1".into(),
                PersistedSession {
                    dialog: vec![entry],
                    original_prompt: Some("fix foo".into()),
                    task_started_at: 1000,
                    last_input_tokens: None,
                },
            );
        }
        store.save_to_disk();

        let store2 = PromptHistoryStore::new(path);
        let restored = store2.get("s1").expect("session should exist");
        assert_eq!(restored.dialog.len(), 1);
        assert_eq!(restored.dialog[0].text, "fix foo");
        assert_eq!(restored.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(restored.task_started_at, 1000);
        assert!(store2.get("nonexistent").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = std::env::temp_dir().join("nonexistent_prompt_history.json");
        let store = PromptHistoryStore::new(path);
        assert!(store.get("any").is_none());
    }

    #[test]
    fn has_any_entries_tracks_inserts_and_removals() {
        let store = PromptHistoryStore::new(PathBuf::new());
        assert!(!store.has_any_entries());
        {
            let mut data = store.data.lock().unwrap();
            data.insert("s1".into(), PersistedSession::default());
        }
        assert!(store.has_any_entries());
        store.remove("s1");
        assert!(!store.has_any_entries());
    }

    #[test]
    fn remove_deletes_session() {
        let store = PromptHistoryStore::new(PathBuf::new());
        {
            let mut data = store.data.lock().unwrap();
            data.insert("s1".into(), PersistedSession::default());
            data.insert("s2".into(), PersistedSession::default());
        }
        store.remove("s1");
        assert!(store.get("s1").is_none());
        assert!(store.get("s2").is_some());
    }
}
