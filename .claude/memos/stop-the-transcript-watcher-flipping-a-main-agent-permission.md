---
created: 2026-09-28 16:55:33
---

# Stop the transcript watcher flipping a main-agent permission dialog from BLOCK back to WORK

A main-agent PermissionRequest (e.g. for Bash) sets the row Blocked 'needs approval: <tool>', but the gated call's tool_use line reaches the transcript at the same moment, and log_watcher::infer_state resolves any non-gating main-session tool_use to Working, so apply_watcher_update promotes the row back to WORK while the dialog is still on screen. Observed in widget.jsonl for chat_id 'claude': classify Blocked 'tool-permission dialog for Bash' at 2026-09-27T03:27:38.979Z, then resume_working at 03:27:39.507Z.

This is the same defect class fixed on 2026-09-27 for AskUserQuestion/ExitPlanMode (infer_state now treats an unanswered adapters::claude::USER_GATING_TOOLS call as deciding no state). That fix does not carry over directly: for a gating tool the answer IS the tool_result, but an approved Bash command writes its tool_result only when the command finishes, so 'only a tool_result promotes' would hold BLOCK through the whole approved run (subagent_gate.rs states the same residual for subagent prompts). Needs a design decision on what marks approval.

Leads to check first: Claude Code's session registry (<claude-config>/sessions/<pid>.json) reported status 'waiting' with waitingFor 'input needed' for a session parked on an AskUserQuestion, which session_registry.rs currently reads as Activity::Unknown (it maps only idle/busy). If the registry reports the same while a permission dialog is open and flips back on approval, that may be the approval signal. Also check whether any hook fires on approval before the tool runs. Reproduce first with a synthetic PermissionRequest plus an appended tool_use line (see .claude/memory/debug_synthetic_hook_events.md), then fix in log_watcher.rs / state.rs.
