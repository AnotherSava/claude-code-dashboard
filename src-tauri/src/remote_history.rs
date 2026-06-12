//! Disk persistence for remote-device dialogs — the peer-session counterpart
//! of `prompt_history.rs`, one file per device under `remote_history/` in the
//! app data dir. Only dialogs are stored: metadata arrives complete with
//! every push, and the receiver's accumulated dialog is otherwise lost on
//! restart (push deltas only carry what's newer than the origin's watermark).
//! Restoration happens at ingest time — the first push from a device after a
//! dashboard restart seeds each session's dialog from disk before the push's
//! deltas merge on top. Entries for sessions absent from later pushes are
//! kept (mirroring `prompt_history`'s keep-forever), so a chat that reopens
//! on the origin restores its prior dialog here too. The history-window
//! catch-up fetch remains the completeness guarantee for gaps disk can't
//! cover (e.g. a fresh install while the origin's watermark is already
//! advanced).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::state::{AgentSession, DialogEntry};

/// One device's persisted dialogs. The device name is repeated inside the
/// file because the filename is sanitized and can't be reversed.
#[derive(Serialize, Deserialize, Default)]
struct DeviceDialogs {
    device: String,
    /// Keyed by namespaced session id ("{device}/{raw_id}"), as held in
    /// `AppState::remote`.
    dialogs: HashMap<String, Vec<DialogEntry>>,
}

pub struct RemoteHistoryStore {
    dir: PathBuf,
    data: Mutex<HashMap<String, DeviceDialogs>>,
}

/// Device names are hostnames from peers' configs — almost always already
/// filesystem-safe, but never trusted: anything outside `[A-Za-z0-9._-]`
/// becomes `_`.
fn sanitize_filename(device: &str) -> String {
    device.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

impl RemoteHistoryStore {
    pub fn new(dir: PathBuf) -> Self {
        let mut data = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| serde_json::from_str::<DeviceDialogs>(&c).map_err(|e| e.to_string())) {
                    Ok(dd) if !dd.device.is_empty() => {
                        data.insert(dd.device.clone(), dd);
                    }
                    Ok(_) => tracing::warn!(path = %path.display(), "remote history file without device name skipped"),
                    Err(e) => tracing::warn!(%e, path = %path.display(), "failed to read remote history"),
                }
            }
        }
        tracing::debug!(devices = data.len(), "remote history loaded");
        Self { dir, data: Mutex::new(data) }
    }

    /// The persisted dialogs for one device, for seeding `sync::ingest`.
    pub fn device_dialogs(&self, device: &str) -> HashMap<String, Vec<DialogEntry>> {
        self.data.lock().unwrap().get(device).map(|dd| dd.dialogs.clone()).unwrap_or_default()
    }

    /// Upsert the given sessions' dialogs into the device's file and write it.
    /// Sessions with empty dialogs and previously stored sessions absent from
    /// `sessions` are left as they are — removal never happens, mirroring
    /// `prompt_history` (bounded by the origin's project count, like local).
    pub fn save_device(&self, device: &str, sessions: &[AgentSession]) {
        let mut data = self.data.lock().unwrap();
        let dd = data.entry(device.to_string()).or_default();
        dd.device = device.to_string();
        for s in sessions.iter().filter(|s| !s.dialog.is_empty()) {
            dd.dialogs.insert(s.id.clone(), s.dialog.clone());
        }
        let path = self.dir.join(format!("{}.json", sanitize_filename(device)));
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(?e, dir = %self.dir.display(), "failed to create remote history dir");
            return;
        }
        match serde_json::to_string(&*dd) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(?e, path = %path.display(), "failed to write remote history");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize remote history"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DialogRole, Status};

    fn entry(text: &str, timestamp: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::User, text: text.into(), timestamp, status: Status::Working, task_start: false }
    }

    fn session(id: &str, dialog: Vec<DialogEntry>) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
            status_from_transcript_scan: false,
            label: "label".into(),
            original_prompt: None,
            task_started_at: 0,
            dialog,
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            display_name: None,
            origin: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("claude_dashboard_remote_history_{tag}_{}", std::process::id()))
    }

    #[test]
    fn round_trip_save_and_load() {
        let dir = temp_dir("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/proj", vec![entry("hello", 10)])]);

        let store2 = RemoteHistoryStore::new(dir.clone());
        let dialogs = store2.device_dialogs("laptop");
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs["laptop/proj"][0].text, "hello");
        assert!(store2.device_dialogs("unknown").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_sessions_keep_their_stored_dialogs() {
        let dir = temp_dir("keep");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/old", vec![entry("kept", 10)])]);
        store.save_device("laptop", &[session("laptop/new", vec![entry("fresh", 20)]), session("laptop/empty", Vec::new())]);

        let dialogs = store.device_dialogs("laptop");
        assert_eq!(dialogs["laptop/old"][0].text, "kept", "absent session survives");
        assert_eq!(dialogs["laptop/new"][0].text, "fresh");
        assert!(!dialogs.contains_key("laptop/empty"), "empty dialogs aren't stored");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn devices_get_separate_files() {
        let dir = temp_dir("files");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop.local", &[session("laptop.local/p", vec![entry("a", 1)])]);
        store.save_device("desk:top", &[session("desk:top/p", vec![entry("b", 2)])]);

        assert!(dir.join("laptop.local.json").exists());
        assert!(dir.join("desk_top.json").exists(), "unsafe chars sanitized");
        let store2 = RemoteHistoryStore::new(dir.clone());
        assert_eq!(store2.device_dialogs("desk:top")["desk:top/p"][0].text, "b", "device name read from file content, not filename");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_loads_empty() {
        let store = RemoteHistoryStore::new(temp_dir("missing_nonexistent"));
        assert!(store.device_dialogs("any").is_empty());
    }
}
