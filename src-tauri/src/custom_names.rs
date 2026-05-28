use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// User-assigned display names, keyed by `chat_id` (the cwd-derived row id).
/// Overrides the chat_id shown in the dashboard. Keyed by chat_id so a name
/// persists across different Claude sessions for the same project. Stored in
/// its own `custom_names.json` (not `config.json`) so it survives the deploy
/// step, which overwrites config.json from the local template.
pub struct CustomNamesStore {
    path: PathBuf,
    data: Mutex<HashMap<String, String>>,
}

impl CustomNamesStore {
    pub fn new(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(?e, path = %path.display(), "failed to read custom names");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        tracing::debug!(names = data.len(), "custom names loaded");
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn get(&self, chat_id: &str) -> Option<String> {
        self.data.lock().unwrap().get(chat_id).cloned()
    }

    /// Sets or, when `name` is empty/whitespace, clears the display name for
    /// `chat_id`. Persists on change.
    pub fn set(&self, chat_id: &str, name: &str) {
        {
            let mut data = self.data.lock().unwrap();
            let trimmed = name.trim();
            if trimmed.is_empty() {
                data.remove(chat_id);
            } else {
                data.insert(chat_id.to_string(), trimmed.to_string());
            }
        }
        self.save_to_disk();
    }

    fn save_to_disk(&self) {
        let data = self.data.lock().unwrap();
        match serde_json::to_string_pretty(&*data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write custom names");
                }
            }
            Err(e) => {
                tracing::warn!(?e, "failed to serialize custom names");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CustomNamesStore {
        let path = std::env::temp_dir().join(format!("custom_names_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        CustomNamesStore::new(path)
    }

    #[test]
    fn set_and_get() {
        let s = store();
        s.set("assistant", "BGA Helper");
        assert_eq!(s.get("assistant").as_deref(), Some("BGA Helper"));
    }

    #[test]
    fn empty_clears() {
        let s = store();
        s.set("assistant", "BGA Helper");
        s.set("assistant", "  ");
        assert_eq!(s.get("assistant"), None);
    }

    #[test]
    fn trims_whitespace() {
        let s = store();
        s.set("a", "  Name  ");
        assert_eq!(s.get("a").as_deref(), Some("Name"));
    }

    #[test]
    fn unknown_is_none() {
        assert_eq!(store().get("nope"), None);
    }
}
