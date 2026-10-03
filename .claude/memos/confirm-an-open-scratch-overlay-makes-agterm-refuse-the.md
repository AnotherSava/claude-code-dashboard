---
created: 2026-10-03 01:39:28
platform: macos
---

# Confirm an open scratch overlay makes agterm refuse the input observation as covered

CHROME asked for this verification on a real Mac and it is the one half that was never done; the predicate is unit-tested only.

Background. agterm_facts::cover() was blind to agterm's scratch overlay: measured on agterm 0.25.0 (commit 94ab03f2) 2026-10-03, opening a scratch overlay leaves the session node's `overlay` at false and instead adds a `surfaces` entry with kind "scratch" and visible true, while the `left` surface goes invisible. Closing it sets that entry's visible to false rather than removing it (the hidden scratch shell stays alive), which is why cover() tests visible and never the entry's presence. The fix and its three-state unit test are in src-tauri/src/terminals/agterm_facts.rs.

What is still unverified is the end-to-end consequence: an input observation taken while a scratch overlay is open over the selected session should be refused as `covered` rather than crediting the session underneath, which would mark it read early.

How to confirm, about fifteen seconds of real use. It needs genuine desktop input, so it cannot be driven from an agent: agtermctl session type writes through the control socket and never produces a CGEvent, so idle::idle_ms does not see it.
1. In agterm, open the scratch terminal on the session you are sitting in.
2. Type a few characters into it.
3. Dismiss it.
Then grep the dashboard's widget.jsonl for decision=attention_poll lines in that window and check the outcome reads `covered`. Scope to the running process: take its start with ps -o lstart= -p $(pgrep -x claude-code-dashboard) and ignore lines before it.

If the outcome is anything else, report the slug to the tauri-dashboard session on CHROME, which owns agterm_facts.rs and asked for this reading.

Separately still unmeasured and not this memo's job: what sets the `overlay` key at all (it stayed false throughout), and whether agterm spells a pane overlay `paneOverlays` - 0.25.0 exposes no pane-overlay command, so none could be opened.
