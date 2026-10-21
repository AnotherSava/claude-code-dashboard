//! Which agterm workspace each project's agent was last seen in, so a session
//! [`crate::session_launcher`] starts for that project opens there rather than
//! in whichever workspace agterm has current at the moment.
//!
//! Keyed by the session's working directory, which is what the launcher holds
//! (the listed directory it starts in) and what agterm's `tree` reports. Filled
//! from every `tree` walk the agterm adapter makes, so the record is current
//! however the session ended: an exit, an idle close or a closed tab all leave
//! the last sighting standing.
//!
//! Its own file rather than a `config.json` field, for the reason
//! [`crate::custom_names`] gives: deploy overwrites `config.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "agterm_workspaces.json";

/// Where a project's agent session sat. A workspace belongs to one window, so
/// its id means nothing without the window's; the name is what survives the
/// workspace being deleted, and is what it is recreated under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastWorkspace {
    pub window: String,
    pub workspace: String,
    pub name: String,
}

pub struct WorkspaceMemory {
    path: PathBuf,
    data: Mutex<BTreeMap<String, LastWorkspace>>,
}

impl WorkspaceMemory {
    pub fn new(path: PathBuf) -> Self {
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(?e, path = %path.display(), "unparseable workspace memory; starting empty");
                BTreeMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "failed to read workspace memory");
                BTreeMap::new()
            }
        };
        Self { path, data: Mutex::new(data) }
    }

    pub fn get(&self, dir: &str) -> Option<LastWorkspace> {
        self.data.lock().unwrap().get(dir).cloned()
    }

    /// Record one complete pass of sightings, `(cwd, where)`, persisting only
    /// when something moved.
    ///
    /// A directory seen in two different workspaces in the same pass keeps its
    /// previous record: there is no telling which of the two is the project's,
    /// and taking whichever was walked last would flip the record, and rewrite
    /// the file, on every pass.
    // Called only by the agterm adapter's tree walk, which is macOS-only.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn observe(&self, sightings: Vec<(String, LastWorkspace)>) {
        // The write happens under the lock, so two walks that each change the
        // map cannot land on disk in the reverse of the order they applied.
        let mut data = self.data.lock().unwrap();
        if apply(&mut data, sightings) {
            self.save(&data);
        }
    }

    fn save(&self, data: &BTreeMap<String, LastWorkspace>) {
        match serde_json::to_string_pretty(data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write workspace memory");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize workspace memory"),
        }
    }
}

/// The pure half of [`WorkspaceMemory::observe`]: whether `data` changed.
fn apply(data: &mut BTreeMap<String, LastWorkspace>, sightings: Vec<(String, LastWorkspace)>) -> bool {
    let mut pass: BTreeMap<String, Option<LastWorkspace>> = BTreeMap::new();
    for (dir, seen) in sightings {
        pass.entry(dir).and_modify(|held| if held.as_ref() != Some(&seen) { *held = None }).or_insert(Some(seen));
    }
    let mut changed = false;
    for (dir, seen) in pass {
        let Some(seen) = seen else { continue };
        if data.get(&dir) != Some(&seen) {
            data.insert(dir, seen);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(window: &str, workspace: &str, name: &str) -> LastWorkspace {
        LastWorkspace { window: window.into(), workspace: workspace.into(), name: name.into() }
    }

    #[test]
    fn a_sighting_is_recorded_and_a_repeat_changes_nothing() {
        let mut data = BTreeMap::new();
        assert!(apply(&mut data, vec![("/p/a".into(), at("W", "S1", "web"))]));
        assert!(!apply(&mut data, vec![("/p/a".into(), at("W", "S1", "web"))]));
        assert_eq!(data.get("/p/a"), Some(&at("W", "S1", "web")));
    }

    #[test]
    fn a_move_to_another_workspace_replaces_the_record() {
        let mut data = BTreeMap::new();
        apply(&mut data, vec![("/p/a".into(), at("W", "S1", "web"))]);
        assert!(apply(&mut data, vec![("/p/a".into(), at("W", "S2", "apps"))]));
        assert_eq!(data.get("/p/a"), Some(&at("W", "S2", "apps")));
    }

    #[test]
    fn two_workspaces_in_one_pass_keep_the_previous_record() {
        let mut data = BTreeMap::new();
        apply(&mut data, vec![("/p/a".into(), at("W", "S1", "web"))]);
        assert!(!apply(&mut data, vec![("/p/a".into(), at("W", "S2", "apps")), ("/p/a".into(), at("W", "S3", "common"))]));
        assert_eq!(data.get("/p/a"), Some(&at("W", "S1", "web")));
    }

    #[test]
    fn an_absent_directory_keeps_its_record() {
        let mut data = BTreeMap::new();
        apply(&mut data, vec![("/p/a".into(), at("W", "S1", "web"))]);
        assert!(!apply(&mut data, Vec::new()));
        assert!(data.contains_key("/p/a"));
    }

    #[test]
    fn the_record_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("workspace_memory_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        WorkspaceMemory::new(path.clone()).observe(vec![("/p/a".into(), at("W", "S1", "web"))]);
        assert_eq!(WorkspaceMemory::new(path.clone()).get("/p/a"), Some(at("W", "S1", "web")));
        let _ = std::fs::remove_file(&path);
    }
}
