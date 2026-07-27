---
name: feedback_instrument_first_fragile
description: For an unreproducible bug in a fragile subsystem with a history of reverted fixes, instrument to capture a real repro FIRST — don't ship a speculative fix on spec
metadata:
  type: feedback
---

When a bug can't currently be reproduced **and** lives in a subsystem with a history of reverted "confident" fixes, land ground-truth instrumentation to capture a real repro before writing any fix. Do not ship a speculative fix on spec, even a low-risk one.

**Why:** On 2026-07-24, offered "ship the backstop fix now" vs "instrument first, then fix" for the auto-resize mixed-DPI scrollbar, the user chose instrument-first. The auto-resize subsystem has burned several fixes that each traded one failure for another (see [[auto_resize_dpr_flicker_collapse]]), and the initial diagnosis had already been wrong once (a probe artifact — [[debug_dpi_unaware_probe_virtualization]]). Shipping a change you can't validate against a repro is exactly the pattern that produced those reverts.

**How to apply:** Sharpens the existing "diagnose via widget.jsonl, don't theorize" ethos ([[debug_state_transitions_via_widget_jsonl]]) with a concrete trigger. Add self-diagnosing logging (requested-vs-actual, fresh-OS-truth beside cached values), deploy, and wait for the failure to recur in the log before implementing — then fix against confirmed evidence. Applies most to the DPI/auto-resize family; a readily reproducible bug still gets fixed directly.
