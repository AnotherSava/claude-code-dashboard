---
created: 2026-09-24 14:17:15
---

# Sync the tray's boolean checkboxes from config_watcher, so a hand edit to config.json moves the tick

Every boolean tray checkbox shows a stale tick after config.json is edited by hand, until the app restarts. The setting itself takes effect — config_watcher reloads the config and consumers read a fresh snapshot — so only the check-mark lies, which is the worse half: the menu says the feature is on while it is off.

Verified 2026-09-24: config_watcher.rs makes exactly one tray call, `sync_lid_awake_title` (for the lid submenu's TITLE, not a tick), and no `set_checked` anywhere. All 26 `set_checked` call sites live in tray.rs, inside click handlers or `sync_*` helpers that only the click path and lid_awake's 5s tick reach.

The affected set is every `toggle_*` in tray.rs:
  toggle_always_on_top, toggle_save_position, toggle_terminal_titles,
  toggle_compact_mode, toggle_idle_awake, toggle_context_alert, toggle_high_alert

`toggle_idle_awake` is the newest member, added with the idle-sleep assertion feature; the hole is pre-existing and was matched deliberately rather than fixed one-off, so the whole class is still open.

Shape of the fix: a `pub fn sync_bool_checks(app: &AppHandle, cfg: &Config)` in tray.rs that pushes `set_checked` for all seven from a config snapshot, called once from config_watcher.rs beside the existing `sync_lid_awake_title` call. One call site, no per-field edge-triggering needed — `set_checked` on an unchanged value is a no-op.

Two things to keep in mind while doing it:
- The tray toggle path does NOT reach config_watcher: `save_to_disk` writes the file, and config_watcher's serialized-equality skip then swallows its own event. So the helper must not be the only thing setting the tick; the existing inline `set_checked` in each toggle stays.
- `sync_lid_awake_state` holds `handles.lid_awake_label` across a `set_text` main-thread hop (tray.rs ~line 639). That is safe today only because nothing else takes that mutex. A new helper must not take a shared handle mutex that a sync command can also reach, or it becomes the deadlock shape commands.rs:1048 documents.
