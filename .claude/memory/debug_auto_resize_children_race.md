---
name: debug_auto_resize_children_race
description: Auto-resize stuck scrollbar (window shorter than content) was the children-sum measurement racing Svelte reconciliation; distinct from DPI drift
metadata:
  type: project
---

A *second*, DPI-independent auto-resize failure (distinct from [[debug_auto_resize_dpi_drift]]): the window settles **shorter than its content, leaving a stuck vertical scrollbar** (a row scrolled out of view). Symptom in `widget.jsonl`: `auto_resize measure` `desired` is ~one row (40px) too small for the rendered content, and no later measure corrects it (overflow check needs a trigger to fire; after a self-resize the `window` 'resize' echo is suppressed, so a quiet session set stays stuck).

**Root cause:** `App.svelte`'s `measureAndSend` summed `.list` children via `getBoundingClientRect()`. Iterating the live `HTMLCollection` races Svelte's keyed-`each` reconciliation — caught a `child_count` that disagreed with the list's own `scrollHeight` (e.g. n=2 children summing 80px while `scrollHeight`=121px = 3 rows), so `desired` came out one row short. Triggered by any genuine session-count change (a flapping remote sync peer, or normal sessions coming/going).

**Fix:** wrap the rows in a non-stretching `.list-inner` (shrink-wraps content) and measure that single element's `getBoundingClientRect().height` — one consistent read, no children-sum race, and immune to the `flex:1` scroll viewport stretching `scrollHeight`. Plus a `ResizeObserver` on the content element (`.list-inner`/`.panel`/`.empty`) as a backstop so any content-height change re-triggers a measure even when no `$effect` dep fired. Observed element shrink-wraps, so window resizes don't feed back a loop.

**Diagnostic method (reusable):** add a temp `frontendLog('debug', ...)` in `measureAndSend` dumping `child_count` / `child_heights` / `list.scrollHeight` / `list.clientHeight` / `scrollbar_w` (`offsetWidth-clientWidth`) / `inner_h` / `desired`. Drive synthetic sessions via direct POST to `/api/event` (see [[debug_synthetic_hook_events]]) — a `keepA` session plus an add/remove `flapB` at irregular intervals reproduces the race. Confirm the fix with `sb_overflow = list.scrollHeight - list.clientHeight`: every scrollbar frame must be immediately followed by `sb_overflow=0` (corrected), never stuck. Note: real concurrent Claude windows (e.g. a `travel` project session) also flap the count during testing — not a bug.
