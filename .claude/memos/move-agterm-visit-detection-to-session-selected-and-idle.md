---
created: 2026-11-03 06:26:50
platform: macos
---

# Move agterm visit detection to session.selected and idle close to session close --undoable

agterm shipped what the 2026-09-02 asks wanted (reported 2026-11-03 by the agterm session; details in the attention-visit-detection-design memory). Running app was 0.36.0 that day, so v0.39+ items wait on an agterm upgrade.

- Departures: replace AgtermAdapter::watch's fsevent watch on agterm's windows/<id>.json snapshots with `agtermctl events --kind session.selected --json` (v0.35), resuming with --run/--after after a reconnect. The payload names `previous`, so the departed session needs no extra `tree` call, and the 300ms save-debounce coalescing loss goes away. Placing the switch for terminals::person_verdict still needs an instant: check whether the event carries a timestamp.
- Poll: `lastSelectedAt` per session in `tree --json` (v0.39) could let the poll see a visit between two ticks. It is absent on sessions restored from older builds.
- Idle close: `session close --undoable` (v0.39) gives the ⌘W undo grace; read `result.undoable` to log which kind of close happened. Builds the argv in agterm_wire beside close_refusal.
- Also: `pane.focused` (v0.38) for split-pane visits, which session.selected does not cover; `tree.changed` (v0.40) for context/overlay changes.
