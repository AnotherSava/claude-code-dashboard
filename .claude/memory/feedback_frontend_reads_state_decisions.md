---
name: feedback_frontend_reads_state_decisions
description: "Frontend reads Rust state-machine decisions (task boundaries, sticky labels) from stamped data fields; it must not re-derive them with its own heuristic — the two drift."
metadata: 
  node_type: memory
  type: feedback
---

The Rust state machine (`state.rs::apply_set` / `label_policy`) is the single owner of sticky-label and task-boundary decisions. When the Svelte frontend needs one of those decisions for display, the backend **stamps it onto the emitted/persisted data** (e.g. `DialogEntry.task_start`) and the frontend reads the field. The frontend must NOT re-implement the decision with a parallel heuristic.

**Why:** a frontend re-derivation drifts from the authoritative Rust logic because it can't see the status-transition history and `continuation_prompts` config the state machine uses. Concretely: the former `dialog.ts::isTaskBoundary` guessed task boundaries from the previous entry's role/text, and a slash-command prompt submitted right after an assistant question *was* the sticky label (Rust: new task) but lost its history highlight + tooltip listing (frontend heuristic: treated it as an answer to the question). Fixed by stamping `task_start` in `apply_set` and deleting `dialog.ts`. This is the [[feedback_eliminate_bug_class]] pattern — remove the duplicate logic, don't patch the heuristic.

**How to apply:** when a display needs a state-machine decision, add a field the backend fills at decision time, not a frontend re-computation. This is the case where a boolean flag IS justified despite [[feedback_no_redundant_flags]] — the frontend genuinely can't reconstruct the decision from per-entry data. Caveat: such a flag is stamped at event time, so it only applies going forward; pre-existing persisted entries won't carry it and can't be reliably backfilled (the deciding context wasn't recorded).
