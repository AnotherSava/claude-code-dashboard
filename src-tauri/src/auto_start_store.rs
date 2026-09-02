//! Which projects a relayed message may start a session for, on this machine.
//!
//! A `chat_id → absolute directory` map. Empty (the default, and the state of a
//! fresh install) means no message ever starts anything.
//!
//! **It is a map rather than a switch** because the target arrives as a
//! `derive_chat_id` output, which is many-to-one — with `projects_root` unset it
//! is the bare folder name, and a real tree holds several `web`s. Nothing in an
//! id says which directory it means, so a boolean could only be honoured by
//! guessing; and the one index that does hold real paths, Claude Code's
//! `~/.claude.json`, is not a curated list — it accumulates every directory a
//! session ever ran in, `system32` among them. Naming the directory *is* the
//! grant, which is why it is written down per machine.
//!
//! **It is its own file, not a `config.json` field**, for the reason
//! [`crate::custom_names`] is: the deploy step overwrites `config.json` from the
//! repo's `config/local.json` template on every run. An entry written here by an
//! approval would vanish at the user's next deploy, silently, and the first sign
//! would be a project they had already approved asking again.
//!
//! Entries arrive two ways and both land here, so there is one list rather than
//! a config half and a runtime half that can disagree: the user edits the file,
//! or an approved request writes one through [`AutoStartStore::grant`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Where the grants live, relative to the app data directory.
pub const FILE_NAME: &str = "auto_start.json";

pub struct AutoStartStore {
    path: PathBuf,
    /// Serializes read-modify-write, and holds the last good read so an
    /// unreadable file degrades to what was granted rather than to nothing
    /// mid-session.
    last_good: Mutex<BTreeMap<String, String>>,
}

impl AutoStartStore {
    pub fn new(path: PathBuf) -> Self {
        let store = Self { path, last_good: Mutex::new(BTreeMap::new()) };
        let loaded = store.snapshot();
        tracing::debug!(projects = loaded.len(), path = %store.path.display(), "startable projects loaded");
        store
    }

    /// The list as it is on disk **right now**.
    ///
    /// Re-read rather than cached, because the file is the user interface: the
    /// tray opens it, and withdrawing a grant means deleting a line. A cached
    /// copy would leave a project startable until the next restart, which is
    /// exactly the wrong direction for the one operation that takes permission
    /// away. It is read only when a message arrives for a project with nothing
    /// running, which is rare enough to pay for a small file read.
    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.read().unwrap_or_else(|degraded| degraded)
    }

    /// The read behind [`Self::snapshot`], keeping *whether the file was
    /// understood*. `Ok` is what is on disk; `Err` carries the same best-effort
    /// list but says it is a fallback.
    ///
    /// [`Self::grant`] needs the distinction and `snapshot`'s callers do not: a
    /// read that fell back is fine to gate a start with, and catastrophic to
    /// write over, because writing `fallback + one entry` over a file we could
    /// not parse replaces the user's whole list with a single line.
    fn read(&self) -> Result<BTreeMap<String, String>, BTreeMap<String, String>> {
        let mut last_good = self.last_good.lock().unwrap();
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => match serde_json::from_str::<BTreeMap<String, String>>(&contents) {
                Ok(parsed) => {
                    *last_good = parsed.clone();
                    Ok(parsed)
                }
                Err(e) => {
                    // Left on disk untouched: overwriting a hand-edited list
                    // over a stray comma would destroy it. The previous good
                    // read stands, so a mid-edit save cannot silently revoke
                    // every grant.
                    tracing::warn!(?e, path = %self.path.display(), "auto_start.json could not be parsed; using the last good read");
                    Err(last_good.clone())
                }
            },
            // No file is the default state and means nothing is startable. An
            // unreadable one keeps what we last saw rather than inventing an
            // absence.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => {
                tracing::warn!(?e, path = %self.path.display(), "failed to read auto_start.json");
                Err(last_good.clone())
            }
        }
    }

    /// Record that `project` may be started at `dir`.
    ///
    /// Idempotent, and it does **not** validate: every caller must have run
    /// [`crate::session_launcher::check_startable`] against the same pair first,
    /// because those checks are what make the entry mean anything and they have
    /// to run on the machine the directory is on. Returns whether the list
    /// actually changed, so a real grant logs differently from a repeat.
    pub fn grant(&self, project: &str, dir: &str) -> bool {
        // Never write over a file we could not read. The alternative is worse
        // than refusing: a fallback list plus the new entry, written over a
        // hand-edited file that merely had a stray comma in it, silently
        // replaces every other grant with this one.
        let Ok(mut current) = self.read() else {
            tracing::warn!(path = %self.path.display(), project, "refusing to record a grant over an auto_start.json this build could not read");
            return false;
        };
        if current.get(project).map(String::as_str) == Some(dir) {
            return false;
        }
        current.insert(project.to_string(), dir.to_string());
        match serde_json::to_string_pretty(&current) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, format!("{json}\n")) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write auto_start.json");
                    return false;
                }
                *self.last_good.lock().unwrap() = current;
                true
            }
            Err(e) => {
                tracing::warn!(?e, "failed to serialize the startable project list");
                false
            }
        }
    }

    /// The file itself, so the tray can open it. Withdrawing a grant is
    /// deleting a line here — there is deliberately no `revoke` method, because
    /// a second way to edit the list would be a second source of truth for it.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AutoStartStore {
        // Unique path per call, for the reason `custom_names`' own helper gives:
        // these run in parallel under one pid, so a shared path lets one test's
        // write race another's read.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("auto_start_test_{}_{}.json", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&path);
        AutoStartStore::new(path)
    }

    /// The default is off, and off means nothing is startable.
    #[test]
    fn a_fresh_store_grants_nothing() {
        assert!(store().snapshot().is_empty());
    }

    #[test]
    fn a_grant_is_recorded_and_survives_a_reload() {
        let s = store();
        assert!(s.grant("transcripts", "/p/transcripts"), "a new entry is a change");
        assert!(!s.grant("transcripts", "/p/transcripts"), "the same entry again is not");
        let reloaded = AutoStartStore::new(s.path().to_path_buf());
        assert_eq!(reloaded.snapshot().get("transcripts").map(String::as_str), Some("/p/transcripts"));
    }

    /// Re-granting the same project at a different directory moves it rather
    /// than keeping both, so the list can never hold two answers for one id.
    #[test]
    fn re_granting_replaces_the_directory() {
        let s = store();
        s.grant("transcripts", "/p/old");
        assert!(s.grant("transcripts", "/p/new"));
        assert_eq!(s.snapshot().get("transcripts").map(String::as_str), Some("/p/new"));
    }

    /// Withdrawing a grant is editing the file, so a grant must not outlive an
    /// edit that removed it — a cached list would keep the project startable
    /// until the next restart, which is the wrong direction for the one
    /// operation that takes permission away.
    #[test]
    fn an_edit_to_the_file_takes_effect_without_a_restart() {
        let s = store();
        s.grant("transcripts", "/p/transcripts");
        std::fs::write(s.path(), "{}").unwrap();
        assert!(s.snapshot().is_empty(), "the same store, no restart, reads the withdrawal");
    }

    /// An unparseable file must read as "nothing is granted" and must not be
    /// overwritten — a stray comma in a hand-edited list should cost a warning,
    /// not the list.
    #[test]
    fn an_unparseable_file_grants_nothing_and_is_left_alone() {
        let path = std::env::temp_dir().join(format!("auto_start_bad_{}.json", std::process::id()));
        std::fs::write(&path, "{ this is not json").unwrap();
        let s = AutoStartStore::new(path.clone());
        assert!(s.snapshot().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ this is not json", "the user's file is still theirs");
        let _ = std::fs::remove_file(&path);
    }

    /// A grant must never write over a file it could not read: the fallback list
    /// plus one entry, saved over a hand-edited file with a stray comma in it,
    /// silently replaces every other grant with the new one.
    #[test]
    fn a_grant_refuses_to_overwrite_an_unreadable_list() {
        let path = std::env::temp_dir().join(format!("auto_start_nowrite_{}.json", std::process::id()));
        std::fs::write(&path, "{\"transcripts\": \"/p/t\", }").unwrap();
        let s = AutoStartStore::new(path.clone());
        assert!(!s.grant("scheduler", "/p/s"), "the grant is refused, not written");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"transcripts\": \"/p/t\", }", "the user's list is untouched");
        let _ = std::fs::remove_file(&path);
    }
}
