//! Loading and saving the JSON files the dashboard keeps in its app data
//! directory, one copy so a fix to how they persist reaches every store that
//! writes through it. `config.json` is the one file written elsewhere
//! (`Config::save_to_disk`): its callers need the write's error, and
//! `config_watcher` watches it for hand edits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// The value stored at `path`, or the empty value where there is none. A file
/// that cannot be read or parsed is logged and treated as empty, since a store
/// that refused to start would take the dashboard down with it; `what` names
/// the store in the log line.
pub fn load<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(?e, path = %path.display(), "unparseable {what}; starting empty");
            T::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            tracing::warn!(?e, path = %path.display(), "failed to read {what}");
            T::default()
        }
    }
}

/// Where a per-device store keeps `device`'s file inside `dir`, the one
/// mapping [`load_per_device`] reads back. Device names are hostnames from
/// peers' configs, almost always filesystem-safe but never trusted: anything
/// outside `[A-Za-z0-9._-]` becomes `_`, so the name cannot be recovered from
/// the path and each file repeats it inside.
pub fn device_path(dir: &Path, device: &str) -> PathBuf {
    let safe: String = device.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    dir.join(format!("{safe}.json"))
}

/// Every device file in `dir`, keyed by the device name `device_of` reads from
/// inside it. A file that cannot be read or parsed, or names no device, is
/// logged and skipped, for the reason [`load`] treats one as empty; a missing
/// directory is no devices. `what` names the store in the log lines.
pub fn load_per_device<T: DeserializeOwned>(dir: &Path, what: &str, device_of: impl Fn(&T) -> &str) -> HashMap<String, T> {
    let mut data = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return data };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| serde_json::from_str::<T>(&c).map_err(|e| e.to_string())) {
            Ok(value) => match device_of(&value) {
                "" => tracing::warn!(path = %path.display(), "{what} file without device name skipped"),
                device => {
                    data.insert(device.to_string(), value);
                }
            },
            Err(e) => tracing::warn!(%e, path = %path.display(), "failed to read {what}"),
        }
    }
    tracing::debug!(devices = data.len(), "{what} loaded");
    data
}

/// Write `data` to `path`, pretty-printed for a file a person may read, through
/// [`write_atomic`].
pub fn save<T: Serialize + ?Sized>(path: &Path, data: &T, what: &str) {
    write_atomic(path, serde_json::to_string_pretty(data), what);
}

/// Write `data` to `path` on one line, for a large file nobody reads by hand
/// (a peer's dialogs, a token cursor), through [`write_atomic`].
pub fn save_compact<T: Serialize + ?Sized>(path: &Path, data: &T, what: &str) {
    write_atomic(path, serde_json::to_string(data), what);
}

/// Write `json` to `path` through a sibling temp file renamed over it,
/// creating the parent directory first. A plain write truncates the file
/// first, so a crash mid-write leaves a torn file that [`load`] reads back as
/// empty, dropping everything the store held; a rename on the same file
/// system is atomic. Callers save under the lock guarding the data, which
/// keeps overlapping saves of one store ordered.
fn write_atomic(path: &Path, json: serde_json::Result<String>, what: &str) {
    let json = match json {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(?e, "failed to serialize {what}");
            return;
        }
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(?e, dir = %dir.display(), "failed to create the directory for {what}");
            return;
        }
    }
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, json.as_bytes()) {
        tracing::warn!(?e, path = %tmp.display(), "failed to write {what}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(?e, path = %path.display(), "failed to persist {what}");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_value_loads_back_and_leaves_no_temp_file() {
        let path = std::env::temp_dir().join(format!("json_store_roundtrip_{}.json", std::process::id()));
        save(&path, &vec![1, 2, 3], "test list");
        assert_eq!(load::<Vec<i32>>(&path, "test list"), vec![1, 2, 3]);
        assert!(!path.with_extension("tmp").exists());
        save_compact(&path, &vec![4, 5], "test list");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[4,5]");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_or_torn_file_loads_empty() {
        let path = std::env::temp_dir().join(format!("json_store_torn_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(load::<Vec<i32>>(&path, "test list").is_empty());
        std::fs::write(&path, "[1, 2").unwrap();
        assert!(load::<Vec<i32>>(&path, "test list").is_empty());
        let _ = std::fs::remove_file(path);
    }
}
