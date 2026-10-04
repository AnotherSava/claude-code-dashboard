use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::project_rename::{rename_key, KeyMove};
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
                delegated_task: session.delegated_task.clone(),
                message_line: session.message_line.clone(),
                task_started_at: session.task_started_at,
            },
        );
    }

    /// Move one row's persisted history to another row id, for a project whose
    /// folder was renamed. Writes to disk only when something moved.
    ///
    /// Refuses where `to` already holds history rather than merging the two: a
    /// second project deriving the same id is the one way that happens, and
    /// interleaving two projects' dialogs cannot be undone.
    pub fn rename(&self, from: &str, to: &str) -> KeyMove {
        let outcome = rename_key(&mut self.data.lock().unwrap(), from, to);
        if outcome == KeyMove::Moved {
            self.save_to_disk();
        }
        outcome
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
            boundary: None,
        };
        {
            let mut data = store.data.lock().unwrap();
            data.insert(
                "s1".into(),
                PersistedSession {
                    dialog: vec![entry],
                    original_prompt: Some("fix foo".into()),
                    delegated_task: None,
                    message_line: None,
                    task_started_at: 1000,
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
    fn rename_persists_the_moved_history() {
        let path = std::env::temp_dir().join(format!("claude_dashboard_prompt_history_rename_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = PromptHistoryStore::new(path.clone());
        store.data.lock().unwrap().insert("my-app".into(), PersistedSession { original_prompt: Some("task".into()), ..Default::default() });
        assert_eq!(store.rename("my-app", "my-app-renamed"), KeyMove::Moved);
        let reloaded = PromptHistoryStore::new(path.clone());
        assert_eq!(reloaded.get("my-app-renamed").and_then(|p| p.original_prompt).as_deref(), Some("task"));
        assert!(reloaded.get("my-app").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = std::env::temp_dir().join("nonexistent_prompt_history.json");
        let store = PromptHistoryStore::new(path);
        assert!(store.get("any").is_none());
    }

    #[test]
    fn has_any_entries_tracks_inserts() {
        let store = PromptHistoryStore::new(PathBuf::new());
        assert!(!store.has_any_entries());
        {
            let mut data = store.data.lock().unwrap();
            data.insert("s1".into(), PersistedSession::default());
        }
        assert!(store.has_any_entries());
    }
}
