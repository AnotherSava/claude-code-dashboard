---
created: 2026-06-27 16:30:00
---

# Harden is_a_question (adapters/claude.rs)

single-pass rule matching (first match wins, no priority), paragraph split on \n\n only, 160-char tail-truncated evidence snippet, and empirically-grown PERMISSION_SEEKING/HANDBACK phrase lists. Now that it runs once on the authoritative Stop.last_assistant_message (no more dual stale/flushed reads), accuracy here is the whole verdict. Validate any change against prompt_history.json per the validate_detection_against_history memory; prefer phrase-matching over broad structural rules. [DONE: audited all 53 real assistant turns in the corpus — 0 false positives, 1 false negative (hand-back question in the second-to-last paragraph trailed by a 'Then I'll …' outro). Fixed with a phrase-gated TRAILING_OUTRO_OPENERS look-back; self-answered case stays Done.]
