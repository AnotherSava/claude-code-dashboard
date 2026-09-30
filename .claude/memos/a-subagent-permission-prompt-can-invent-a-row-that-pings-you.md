---
created: 2026-09-30 13:52:31
---

# A subagent permission prompt can invent a row that pings you DONE for a turn that never ran

Introduced 2026-09-30 by the DONE/IDLE redefinition, found by that change set's own review and reported rather than guarded. Rare, and the cost is one misleading phone buzz rather than anything lost.

THE PATH. `AppState::open_subagent_prompt` has a branch that CREATES a row: a subagent's `PermissionRequest` arrives for a chat_id the dashboard holds no row for, so one is invented to hang the prompt on. That row needs a base state to sit under the BLOCK overlay, and before this change set the base was `Status::Idle`, which no notification rule fires on by default. It is now `Status::Done`, because `Done` became the evidence-free sink and `Idle` became a positive claim that there is nothing to come back to — which an invented row certainly has not established.

`settle_subagent_prompts` then writes that base back VERBATIM when the dialog closes, clock included, by design: the row is meant to read as if the prompt had never opened. So the row lands on `Done`, stamped with the prompt-open instant.

WHAT FIRES. `config.rs` ships `states.done` at `afk_window_ms: 60_000`. `notifications::reconcile` sees a `Done` row, the user idle 60s, and idle >= time-in-state, and sends `build_message_text`, which is `[<name>] done` followed by the last dialog text — empty here, since the row has no dialog. So the phone gets a bare `[some-project] done` for a session that never ran a turn.

Nothing declines it: `state_observed_here` only refuses rows whose state predates process start, and this row is created live.

WHY IT IS RARE. It needs all three at once — a project with no row at all (so not one the dashboard has seen this session), a subagent permission prompt opening AND settling there, and the user away for 60s with no other event on the row. It also cannot happen for any project already on the widget, which is most of them.

TWO FIXES, neither obviously right, which is why this is parked rather than done:
- Mark the invented row. It is the one row in the system provably without a finished turn, so a flag set in `open_subagent_prompt`'s create branch could make `fire_reason` decline it. Costs a field for a rare case, which `feedback_no_redundant_flags` would push back on.
- Widen `state_observed_here`. It exists to stop a restored row pinging on a state the previous process already announced; "a row this process invented rather than classified" is arguably the same idea. Cheaper, and it puts the decline where the other decline already lives.

DO NOT fix it by sending the base back to `Idle`. That reopens the forbidden direction the whole redefinition closed — a row claiming there is nothing to come back to on no evidence — and `state.rs`'s `nothing_evidence_free_can_produce_a_clean_row` test asserts against exactly that, so it would fail loudly and correctly.

Read `state.rs`'s `Status::Done` doc comment and `open_subagent_prompt`'s base-status comment before touching this; both explain why the base is what it is.
