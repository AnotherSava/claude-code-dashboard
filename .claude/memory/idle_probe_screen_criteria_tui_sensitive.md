---
name: idle_probe_screen_criteria_tui_sensitive
description: RETIRED — idle_probe screen-scrape removed 2026-07-01; instant Esc-cancels DO write the [Request interrupted by user] marker (verified), so the marker path is authoritative
metadata:
  type: project
---

**Retired 2026-07-01: `idle_probe.rs` deleted.** The screen classifier (`classify` / `BUSY_MARKERS` / `has_active_spinner` / `has_active_timer`) read the live Claude Code TUI to spot Claude's idle prompt and revert a `Working` row on an instant Esc-cancel. Removed for two reasons:

1. **Its premise was empirically false.** It assumed an instant Esc-cancel "writes nothing to the transcript." Real transcripts show Claude Code writes `[Request interrupted by user]` even for **sub-second, zero-output** cancels (verified across the corpus: 0.65 / 0.69 / 0.71 / 0.92 / 0.94s cases). So `log_watcher`'s marker path is the sole, cross-platform, authoritative Esc-cancel detector — no screen-scrape needed.
2. **It couldn't distinguish a cancel from a slow silent turn.** A legitimately slow turn that hasn't emitted output yet shows the *same* idle-prompt-over-quiet-transcript picture as an instant cancel, so it false-reverted live turns to BLOCK — a bug no grace window could fix (the `printlab` incident: a real turn stayed silent 2m25s and got reverted at ~8s).

Don't reintroduce terminal-screen state inference for cancels. If a genuine no-marker instant cancel ever surfaces, the fallback is a `has_seen_busy`-gated probe (only confirm a busy→idle transition, never guess an un-started turn), NOT a bigger grace and NOT the raw screen scrape. See [[terminal_promote_to_working_unsafe]].
