---
name: notification_text_mirrors_primary_text
description: Row text is decided once in Rust (AgentSession::primary_text / row_line); the frontend draws the stamped row_line and notifications call primary_text; agwinterm's context line deliberately uses shown_task instead. Never re-add a TS copy.
metadata:
  type: project
---

What a session row shows is decided in one place, `src-tauri/src/state.rs`:

- `AgentSession::primary_text` is the status-aware pick: for `blocked`/`error` the current `label` (the question / approval request), otherwise `shown_task` (the delegated task, else the person's prompt, else the agent message's line, else an excerpt of the envelope), falling back to `label`. `notifications::build_message_text` calls it.
- `AgentSession::row_line` is the row's task line: `primary_text` where non-empty, else the most recent task in the dialog, tagged `Current`/`Past`. `commands::display_snapshot` stamps it onto every row as `row_line`; `SessionItem.svelte` draws that field.
- agwinterm is the deliberate exception: `terminal_title::desired_labels` pairs each row's `display_label` (the name a session's console title, written by `build_title`, is matched against) with `AgentSession::shown_task` as the text for that session's context line, never `primary_text`/`row_line`, so a Blocked row's context line keeps its task instead of reading "has a question". The status is already on the title's glyph. `a_rows_context_is_its_task_on_one_line_never_its_question` pins it.

**Why:** the rule once lived in a TS copy beside the Rust one and they drifted — `build_message_text` used the raw `label`, so a row that went Blocked → Done kept its stale Blocked label and the Telegram ping read `"[printlab] done\nneeds approval: tool"` while the dashboard row showed the task. `label_policy::select` preserves the prior `label` across `Stop`→Done (the `Stop` event carries no label), so only the status-aware pick is right. A second TS copy (the past-task fallback) then disagreed with Rust on what counts as blank (JS `trim` strips U+FEFF, Rust's does not).

**How to apply:** change the rule in `state.rs` only; the frontend has no copy to update. Do not reintroduce a `displayLabel`/`lastTask`-style derivation in TS. Contrast [[feedback_frontend_question_detector_lenient]]: there the frontend's *question detector* deliberately diverges from Rust `is_a_question` — but row-text display must not. Related: [[feedback_frontend_reads_state_decisions]].
