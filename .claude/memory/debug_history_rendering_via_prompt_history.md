---
name: debug-history-rendering-via-prompt-history
description: Debug history-window rendering artifacts by dumping raw stored dialog text from prompt_history.json
metadata:
  type: project
---

When a history-window rendering artifact survives a fix, inspect the stored entry directly: read `%APPDATA%\com.anothersava.claude-code-dashboard\prompt_history.json`, find the dialog entry, and dump per-line lengths/char codes. This separates "the data really contains X" from "a rendering path bypasses the pipeline" — it's how the fold branch-3 bug (character-budget fold returning joined text with embedded `\n`, skipping per-line rendering) was isolated after the first blank-line fix appeared to not work (2026-06-04).

**Why:** the history window has multiple render paths (fold branches, code segments, tables); a fix in one path can leave the symptom alive in another, and guessing wastes deploy cycles.

Related: [[feedback-history-compact-blank-lines]].
