//! Disk persistence for remote-device dialogs — the peer-session counterpart
//! of `prompt_history.rs`, one file per device under `remote_history/` in the
//! app data dir. Only dialogs are stored: metadata arrives complete with every
//! push, while dialog is fetched incrementally, so the accumulated copy would
//! otherwise be lost on restart and re-pulled in full. Restoration happens at
//! ingest time — the first push from a device after a dashboard restart seeds
//! each session's dialog from disk, and what we hold then decides the `since`
//! of the next pull, so a restored copy costs nothing to catch up. Entries for
//! sessions absent from later pushes are kept (mirroring `prompt_history`'s
//! keep-forever), so a chat that reopens on the origin restores its prior
//! dialog here too. The history-window catch-up fetch remains the completeness
//! guarantee for what disk can't cover.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::project_rename::{rename_key, KeyMove, ProjectRename};
use crate::state::{AgentSession, DialogEntry};

/// One device's persisted dialogs. The device name is repeated inside the
/// file because the filename is sanitized and can't be reversed.
#[derive(Serialize, Deserialize, Default)]
struct DeviceDialogs {
    device: String,
    /// Keyed by namespaced session id ("{device}/{raw_id}"), as held in
    /// `AppState::remote`.
    dialogs: HashMap<String, Vec<DialogEntry>>,
    /// The `at` of the newest of this device's project renames already applied
    /// here (`crate::project_rename::ProjectRename`). Every push repeats the
    /// device's recent renames, and an id renamed away can come back into use,
    /// so each is applied once and never again.
    #[serde(default)]
    renames_applied: i64,
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

    /// Apply `device`'s project renames that this store has not applied yet, in
    /// order, re-filing each dialog under the id its project now derives on
    /// that device. Returns the renames newly applied.
    ///
    /// Where the new id already holds a dialog the two are left as they are:
    /// both belong to someone, and choosing one would delete the other.
    pub fn apply_renames(&self, device: &str, renames: &[ProjectRename]) -> Vec<ProjectRename> {
        let mut data = self.data.lock().unwrap();
        let applied_before = data.get(device).map_or(0, |dd| dd.renames_applied);
        let mut fresh: Vec<ProjectRename> = renames.iter().filter(|r| r.at > applied_before).cloned().collect();
        if fresh.is_empty() {
            return fresh;
        }
        fresh.sort_by_key(|r| r.at);
        let dd = data.entry(device.to_string()).or_default();
        dd.device = device.to_string();
        for r in &fresh {
            let moved = rename_key(&mut dd.dialogs, &format!("{device}/{}", r.from), &format!("{device}/{}", r.to));
            if moved == KeyMove::Conflict {
                tracing::warn!(device, from = %r.from, to = %r.to, "a peer's renamed project already has a dialog under its new id here; both kept");
            }
        }
        dd.renames_applied = fresh.last().map_or(applied_before, |r| r.at);
        self.write_device(dd);
        fresh
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
        self.write_device(dd);
    }

    fn write_device(&self, dd: &DeviceDialogs) {
        let path = self.dir.join(format!("{}.json", sanitize_filename(&dd.device)));
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
        DialogEntry { role: DialogRole::User, text: text.into(), timestamp, status: Status::Working, task_start: false, boundary: None }
    }

    fn session(id: &str, dialog: Vec<DialogEntry>) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
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
            waiting_backstop_armed: false,
            display_name: None,
            origin: None,
            instruction_drift: false,
            canary: crate::state::Canary::Off,
            attended_at: None,
            content_seen_at: None,
            origin_label: None,
            turn_from_relay: false,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            subagent_gate: None,
            terminal_stale_at: None,
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

    fn rn(from: &str, to: &str, at: i64) -> ProjectRename {
        ProjectRename { from: from.into(), to: to.into(), at }
    }

    #[test]
    fn a_renamed_project_keeps_its_dialog_here() {
        let dir = temp_dir("rename");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/my-app", vec![entry("old", 10)]), session("laptop/other", vec![entry("x", 5)])]);
        let renames = [rn("my-app", "my-app-renamed", 100)];
        assert_eq!(store.apply_renames("laptop", &renames).len(), 1);
        assert!(store.apply_renames("laptop", &renames).is_empty(), "every push repeats the rename, and it applies once");

        let reloaded = RemoteHistoryStore::new(dir.clone());
        let dialogs = reloaded.device_dialogs("laptop");
        assert_eq!(dialogs["laptop/my-app-renamed"][0].text, "old");
        assert!(!dialogs.contains_key("laptop/my-app"));
        assert_eq!(dialogs["laptop/other"][0].text, "x");
        assert!(reloaded.apply_renames("laptop", &renames).is_empty(), "what was applied survives a restart");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An id renamed away can come back into use at the origin. Its new dialog
    /// must not be carried off by the rename that keeps riding the push.
    #[test]
    fn a_reused_id_is_not_renamed_again() {
        let dir = temp_dir("rename_reuse");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/web", vec![entry("first project", 10)])]);
        store.apply_renames("laptop", &[rn("web", "web-old", 100)]);
        store.save_device("laptop", &[session("laptop/web", vec![entry("a new project", 200)])]);
        assert!(store.apply_renames("laptop", &[rn("web", "web-old", 100)]).is_empty());
        let dialogs = store.device_dialogs("laptop");
        assert_eq!(dialogs["laptop/web"][0].text, "a new project");
        assert_eq!(dialogs["laptop/web-old"][0].text, "first project");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A peer asleep through a swap replays both steps in order.
    #[test]
    fn a_missed_swap_is_replayed_in_order() {
        let dir = temp_dir("rename_swap");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/web", vec![entry("old project", 10)]), session("laptop/web-new", vec![entry("new project", 20)])]);
        store.apply_renames("laptop", &[rn("web-new", "web", 101), rn("web", "web-old", 100)]);
        let dialogs = store.device_dialogs("laptop");
        assert_eq!(dialogs["laptop/web-old"][0].text, "old project");
        assert_eq!(dialogs["laptop/web"][0].text, "new project");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_onto_a_held_dialog_keeps_both() {
        let dir = temp_dir("rename_conflict");
        let _ = std::fs::remove_dir_all(&dir);

        let store = RemoteHistoryStore::new(dir.clone());
        store.save_device("laptop", &[session("laptop/a", vec![entry("a", 10)]), session("laptop/b", vec![entry("b", 20)])]);
        store.apply_renames("laptop", &[rn("a", "b", 100)]);
        let dialogs = store.device_dialogs("laptop");
        assert_eq!((dialogs["laptop/a"][0].text.as_str(), dialogs["laptop/b"][0].text.as_str()), ("a", "b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_loads_empty() {
        let store = RemoteHistoryStore::new(temp_dir("missing_nonexistent"));
        assert!(store.device_dialogs("any").is_empty());
    }
}
