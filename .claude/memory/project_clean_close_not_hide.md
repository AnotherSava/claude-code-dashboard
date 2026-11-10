---
name: project_clean_close_not_hide
description: On both machines a CLEAN session is closed after 30 min and never hidden; don't propose hiding synced CLEAN rows
metadata:
  type: project
---

Since 2026-11-09, by the user's choice, both machines run with `idle_close_ms: 1800000` and `clean_hide_after_ms: null`: a CLEAN session is closed after 30 minutes, and the widget hides nothing. Windows carries it in `config/local.template.json`, the Mac in `config/local.json`, so a deploy keeps it (see [[project_config_wiped_on_deploy]]).

**Why:** the user wants an abandoned session ended rather than out of sight. The question came up when the Mac's widget showed a synced CLEAN row that the Windows widget had hidden, since the hide applies to local rows only.

**How to apply:** don't offer to extend the hide to synced rows or to re-enable it. A CLEAN row that outlives 30 minutes is an idle-close failure to diagnose (`idle_close` outcomes in `widget.jsonl`), not a display question.
