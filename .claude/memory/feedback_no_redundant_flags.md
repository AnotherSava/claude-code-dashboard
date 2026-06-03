---
name: feedback-no-redundant-flags
description: "Don't add boolean flags when existing data values already distinguish all cases"
metadata: 
  node_type: memory
  type: feedback
---

Don't introduce boolean state flags when the shape/values of existing data already discriminate all cases.

**Why:** User pointed out `session_ended` flag was redundant because `last_input_tokens` alone distinguishes new session (big token drop after clear), `--continue` (no drop), and `/compact` (drop goes through existing-session path, updating `last_input_tokens` before any restoration). The flag added complexity without information. Related to [[feedback-favor-clean-design]] but distinct — that's about not keeping legacy fields alongside replacements; this is about not adding new flags when existing data already carries the signal.

**How to apply:** Before adding a boolean flag to distinguish cases, check whether an existing numeric/optional field's values already cover all branches. If they do, use the value check directly.

**Exception:** a flag is NOT redundant when the consumer can't reconstruct the producer's decision from the data available to it (a cross-layer information gap). E.g. `DialogEntry.task_start` is a justified boolean because the Svelte frontend can't reconstruct the Rust state machine's task-boundary decision from per-entry fields. See [[feedback_frontend_reads_state_decisions]].
