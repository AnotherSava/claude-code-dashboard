---
name: remote_store_device_keying
description: Per-device remote_history/usage/tokens files are keyed by the device field INSIDE each file, and one unknown enum variant silently rejects a whole file — so migrate by merging, never by renaming
metadata:
  type: project
---

The `remote_history/`, `remote_usage/` and `remote_tokens/` stores each load with `read_dir` + `data.insert(dd.device.clone(), dd)`, keyed by the **`device` field inside the file**, not by the filename. Two files declaring the same device means whichever is read last silently wins.

**Migrate a device's data by merging into the live file — never by renaming the file or editing its `device` field.** A rename produces exactly the two-files-one-key collision above, and the loser vanishes with no error. Merge with the app **stopped** (it rewrites these files from memory on every content change) and dedupe on `(timestamp, role, text)`, or sessions the two copies share double.

**One unknown enum variant rejects the entire file.** Serde fails the whole `DeviceDialogs`, `RemoteHistoryStore::new` skips it, and the only trace is a `failed to read remote history` WARN — the startup `remote history loaded devices: N` count is the tell. `Mac.json` was unreadable this way for months. Legacy `status: "awaiting"` maps to **`blocked`**, not `waiting`: commit d577efe renamed `Awaiting`→`Blocked` *and* added a separate `Waiting`, so mapping by spelling mislabels the entries.

**Renaming `sync.device_name`** re-namespaces every id (`{device}/{raw_id}`) and starts fresh store files. Put the new key in the receiver's `sync.peer_identity` **before** the rename or attestation drops from `attested` to `claimed`. The re-pull is content-driven and recovers everything the peer still holds — but it is asynchronous and runs for minutes, so file sizes read straight after a rename are mid-flight, not results. Wait for it to settle before concluding anything was lost.

**How to apply:** before deleting any orphaned device file, prove containment against the live one rather than trusting record counts — tokens need a per-id, per-field comparison because `reduce_by_id` takes the max per field, and dialogs can hold entries `merge_dialog_entries` dropped as an apparent transcript re-read (one such entry was recoverable only from the orphan). See [[sync_device_pair]].
