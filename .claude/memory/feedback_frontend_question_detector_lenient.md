---
name: frontend-question-detector-lenient
description: "Frontend isAQuestion in src/lib/dialog.ts is intentionally more lenient than backend is_a_question — don't unify them"
metadata: 
  node_type: memory
  type: feedback
---

`src/lib/dialog.ts::isAQuestion` is a port of `src-tauri/src/adapters/claude.rs::is_a_question`, but **more lenient**: it scans the entire assistant message for permission-seeking phrases + `?`, where Rust only scans the last paragraph.

**Why:** The two detectors have different blast radii:
- Backend `is_a_question` → sets `Status::Blocked` → can trigger Telegram notifications + UI pulse. False-positives are noisy and annoying.
- Frontend `isAQuestion` → suppresses the `.sticky` task-boundary highlight in the history window (via `isTaskBoundary`). False-positives just remove a visual marker; no side effects.

So the frontend should err on the side of detecting a question; the backend should err on the side of not. A real case that motivated the divergence: assistant proposes "Want me to delete X? Proposed: ...\n\n...\n\n— confirm and I'll run it." The user replies "y". Backend correctly doesn't fire Blocked (could be conversational). Frontend should *not* highlight "y" as a new task boundary, so it scans the whole text and catches the earlier "Want me to ... ?" pattern.

**How to apply:**
- If tempted to "fix" the divergence (e.g. DRY-port one to the other), don't — keep them separate with different paragraph-scoping rules.
- If extending detection patterns (new permission-seeking phrases, alternative imperative cues), update both files but consider whether the new pattern should be backend-strict or frontend-lenient.
- The frontend has no `benign_closers` filtering (config-driven on backend) — fine for boundary detection, since benign-closer questions also shouldn't look like task starts.
- Both call sites of `isTaskBoundary`: `src/HistoryApp.svelte` (the `.sticky` class on entries) and `src/lib/components/SessionItem.svelte` (the tooltip `taskPrompts` filter).
