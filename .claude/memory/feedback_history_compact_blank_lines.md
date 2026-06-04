---
name: feedback-history-compact-blank-lines
description: History window drops blank lines inside messages entirely; user rejected even reduced-height gaps
metadata:
  type: feedback
---

History window renders messages compactly: blank lines inside a message are dropped entirely (2026-06-04). The user first asked for reduced-height gaps (0.6em), then rejected even that — full removal, with the block-per-line layout's line-height as the only paragraph separation. Code blocks keep their blank lines (whitespace is meaningful there).

**Why:** empty lines waste vertical space in a dense monitoring view; block layout already gives enough visual separation.

**How to apply:** don't reintroduce paragraph spacing, gaps, or blank-line rendering in `HistoryApp.svelte` without asking. If separation ever feels too tight, the agreed fallback to propose is a small `margin-top` on paragraph starts — not blank lines. When touching fold logic, see [[debug-history-rendering-via-prompt-history]] and keep all fold branches returning per-line arrays so blank-line dropping applies uniformly.
