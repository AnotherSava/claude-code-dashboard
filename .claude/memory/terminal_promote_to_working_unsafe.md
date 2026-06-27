---
name: terminal_promote_to_working_unsafe
description: Reverted — promoting rows to Working from the terminal "esc to interrupt" marker strands them; use the UserPromptExpansion hook
metadata:
  type: project
---

Tried and reverted (2026-06-08): extending `idle_probe` to **promote** a non-Working row to Working when the terminal shows `esc to interrupt` (to flip a skill launch to WORK fast). It caused **stuck-in-WORK** — the marker can be present while a row should be `Blocked`/`Done`, and a one-directional promote strands the row in `Working` that the conservative 2-read demote can't recover.

Terminal-screen reading is a fine *positive-idle* anchor for **demoting** (idle_probe's original, kept role) but NOT a reliable "should be Working" signal. The right fix for skill-launch latency is the **`UserPromptExpansion`** hook — a real event that fires the instant a slash command is invoked, ~15 s before `UserPromptSubmit`; no terminal reading, no stranding. `idle_probe` stays **demote-only**: there is no interrupt/cancel hook (see [[hook_env_var_setup]] / the `claude-code-integration` learning), so it's still required for the instant Esc-cancel.
