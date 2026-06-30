---
name: notification_text_mirrors_primary_text
description: Telegram notification text (build_message_text) must mirror the frontend primaryText label/original_prompt rule; they drifted once
metadata:
  type: project
---

Two separate functions decide what text a session row shows, and they must agree: Rust `notifications::build_message_text` and the frontend `primaryText` (`src/lib/types.ts`).

**The rule:** for `blocked`/`error` show the current `label` (the question / approval request); otherwise show `original_prompt` (the task), falling back to `label`.

They drifted once — `build_message_text` used the raw `label` unconditionally, so a row that went Blocked → Done kept its stale Blocked label and the Telegram ping read `"[printlab] done\nneeds approval: tool"` while the dashboard row correctly showed the task.

**Why:** `label_policy::select` preserves the prior `label` across a `Stop`→Done transition (the `Stop` event carries no label), so a Done row's `label` is frequently a stale Blocked string — only the status-aware pick is right.

**How to apply:** change both places when you change the row-text rule. Contrast [[feedback_frontend_question_detector_lenient]]: there the frontend's *question detector* deliberately diverges from the Rust `is_a_question` — but row-text *display* must NOT diverge. Related: [[feedback_frontend_reads_state_decisions]].
