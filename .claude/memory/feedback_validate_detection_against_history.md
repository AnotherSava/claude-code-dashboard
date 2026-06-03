---
name: validate-detection-against-history
description: Validate awaiting/question detection changes against the real prompt_history.json corpus; prefer precise phrase-matching over broad structural rules
metadata: 
  node_type: memory
  type: feedback
---

When changing the awaiting / question detection in `src-tauri/src/adapters/claude.rs` (`is_a_question` and its phrase lists), validate every candidate criterion against the real `prompt_history.json` corpus (the persisted assistant dialog entries) before adopting it, and prefer precise phrase-matching over broad structural rules.

**Why:** This session we simulated each candidate over ~350 real assistant messages. The user chose the scoped phrase-list approach (permission/save/interrogative phrases + a sentence-initial `Paste …` matcher) over the elegant "first sentence of the last paragraph ends with `?`" rule — even though the structural rule had 0 false positives in-sample — because a plausible *out-of-sample* false positive existed (rhetorical "Why does X? … Fixed it." would flag a finished agent as `awaiting`). A false `awaiting` on a done agent is the costly error to avoid; it erodes trust in the widget.

**How to apply:** For future detection-heuristic changes here: (1) simulate the candidate over `prompt_history.json` and classify hits/misses; (2) weigh out-of-sample FP classes, not just in-sample counts; (3) extend the phrase list (cheap, one line) rather than reach for a broad structural rule. The detection paths are documented in `docs/pages/classification.md`.
