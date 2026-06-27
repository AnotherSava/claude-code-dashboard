---
name: idle_probe_screen_criteria_tui_sensitive
description: idle_probe's screen classifier keys on the live Claude Code TUI (version-sensitive); re-validate flapping by logging the classified-Idle tail to widget.jsonl
metadata:
  type: project
---

`idle_probe.rs`'s `classify` / `BUSY_MARKERS` / `has_active_timer` read the live Claude Code TUI, which shifts across versions — so the busy/idle criteria can silently go stale and cause WORK↔WAIT flapping.

Known traps found so far: (1) typing a prompt mid-turn strips "esc to interrupt" off the spinner line — the spinner clock `(49s …)` (`has_active_timer`) is the composition-proof busy signal; (2) AskUserQuestion / ExitPlanMode selection menus show the input-box `─` border but no spinner → matched via `"to navigate"` / `"Esc to cancel"`.

**How to apply:** if the flapping recurs, re-validate the same way it was found — temporarily log the classified-`Idle` screen tail to `widget.jsonl` (`decision="idle_probe_screen"`, a wider ~30-line window) inside the `Screen::Idle` arm during a real working turn, read the actual screen, then update the criteria from that data rather than guessing. Related: [[terminal_promote_to_working_unsafe]].
