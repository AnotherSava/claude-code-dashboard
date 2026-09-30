---
created: 2026-09-29 17:47:08
---

# Keep runtime config values across a deploy, or offer an option to

A deploy overwrites the installed config.json wholesale, so any setting changed at runtime (a tray checkbox, a hand edit) that is not also in the per-machine source file reverts to its default on the next deploy. Found 2026-09-29: high_alert was switched on from the tray on the Windows box, is true in the live config.json, and is absent from config/local.template.json, so the next deploy turns it off with no warning.

Mechanism: the shared ~/.claude/skills/deploy/scripts/deploy-tauri.sh does cp -f config/local.json "$CONFIG_DEST" (the 'Applied config/local.json' step). On Windows local.json is rendered from config/local.template.json through Doppler by scripts/deploy.sh and deleted afterwards; on the Mac local.json is copied verbatim. See the project memory project_config_wiped_on_deploy.

Wanted: runtime-changed values survive a deploy by default, or at least an option to make them. Open design questions to settle before building:
- Merge instead of overwrite: the source file's keys win, keys present only in the live config are kept. Cost: a key deleted from the source file then lives on forever in config.json, so removal needs its own path.
- Where it lives: the shared deploy script (dotfiles repo, affects every Tauri project) vs this project's scripts/deploy.sh vs the app itself (persist tray-toggled settings in their own app-data file, the way custom_names.json and auto_start.json already are, which is the pattern that memory recommends).
- Which settings count as runtime state: at least the tray checkboxes (high_alert, tray_context_alert_enabled, keep awake, tray_badge, history_font_size).
