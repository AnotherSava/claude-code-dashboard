---
name: terminal_promote_to_working_unsafe
description: Don't reconstruct agent state from the terminal screen — both promote (strands rows) and demote (idle_probe, false-reverts slow silent turns) were tried and removed; use authoritative hooks/markers
metadata:
  type: project
---

Terminal-screen reading was tried in **both** directions to infer agent state, and both were removed — don't reintroduce either.

- **Promote (2026-06-08, reverted):** flipping a non-`Working` row to `Working` when the terminal showed `esc to interrupt` stranded rows in `Working` (the marker can be present while a row should be `Blocked`/`Done`; a one-directional promote the conservative 2-read demote couldn't recover). The right fix for skill-launch latency is the **`UserPromptExpansion`** hook — a real event ~15s before `UserPromptSubmit`, no screen reading, no stranding.
- **Demote (`idle_probe`, removed 2026-07-01):** the demote-only screen-scrape reverted a `Working` row when it positively saw Claude's idle prompt. It false-reverted legitimately slow **silent** turns (same idle-looking screen as an instant cancel — the `printlab` bug), and its premise was false anyway: instant Esc-cancels DO write the `[Request interrupted by user]` marker, so `log_watcher`'s marker path is the authoritative cross-platform cancel detector. See [[idle_probe_screen_criteria_tui_sensitive]].

**Rule:** derive state from authoritative signals (lifecycle hooks, the interrupt marker), never from parsing the terminal TUI. There is no cancel hook, but the transcript marker covers the Esc-cancel; `liveness_reaper` covers cancel-then-`exit`.
