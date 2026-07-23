---
name: feedback_indicator_certainty
description: Status/canary indicators must not over-claim — a "healthy/alive" (green) state requires confirmed evidence; unconfirmed-but-set-up gets a distinct pending color, never green nor the "off" color
metadata:
  type: feedback
---

For the dashboard's status / canary indicators, a positive "healthy" color (green) must reflect a *confirmed* good state — never a merely-plausible or not-yet-checked one.

**Why:** The user pushed back on the instruction-adherence canary coloring the agent name green whenever it was "set up and not-yet-caught-drifting," which lumped in a session whose marker had **never actually been observed** (the injected instruction may not even have reached the model): "why does this one show green? it gives false sense of certainty." Green vouching for something unconfirmed is a false positive.

**How to apply:** Gate the healthy/green state on real evidence (here: the nonce's `seen` bit — the marker echoed at least once). Give "set up but unconfirmed" its own distinct color (amber / pending) — *not* green (false certainty) and *not* the "off/absent" color (which would falsely read as "no indicator at all"). The unknown state is itself a useful signal — an amber that never turns green flags an undelivered setup. Same principle for any 3+-state indicator: off → pending/unknown → alive, and only claim "alive" on confirmation. See CLAUDE.md `http_server.rs` canary (`Canary` enum Off/Pending/Alive/Dead). [[feedback_frontend_reads_state_decisions]]
