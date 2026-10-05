---
created: 2026-10-04 21:29:05
---

# Decide whether an untagged separator should ever read as CLEAN, or stay DONE forever

Five local rows sit on DONE where the dashboard arguably means CLEAN, because their persisted dialog ends with a separator written before boundary tagging existed. Reported by the transcripts session on the other machine, 2026-10-04, from live reads of prompt_history.json and widget.jsonl; nothing was changed.

The code is doing what it says. `resume_is_clean` in http_server.rs defers to `ends_with_clear_boundary` in state.rs, which requires `boundary == Some(Clear)`, and a test pins that an older build's untagged separator is deliberately not evidence. The log line for bga-assistant at that session's start reads:

    decision=classify status=Done
    reason=SessionStart source="resume" -> resumed context; clean only if it ended at a /clear

What the peer measured, each row's dashboard status against the last dialog entry in its prompt_history.json:

    row                dash      last entry     role       boundary
    agterm             idle      10-04 19:37    separator  clear
    tauri-dashboard    idle      10-04 19:32    separator  clear
    bga-assistant      done      09-29 16:05    separator  null
    printlab           done      09-29 16:01    separator  null
    scheduler          done      09-29 17:18    separator  null
    tripit             done      09-29 16:35    separator  null
    travel-map         done      09-14 13:36    separator  null

The two IDLE rows are exactly the two whose last separator carries a Clear boundary.

The decision: accept this as self-healing (a row reaches CLEAN the next time someone types /clear in it), or add a fallback for a dialog carrying no boundary tag anywhere, which is the only shape that distinguishes a pre-tagging dialog from a post-tagging one that genuinely ended on something else. A one-time backfill is the third option and the least attractive, since it writes a verdict into stored data that nothing observed.

Two loose ends the peer found while looking, worth confirming before deciding, since they suggest the separator append is not firing at all rather than only being untagged:

1. bga-assistant was /cleared on 2026-09-30 at 14:50 local and its dialog's last entry predates that by a day, so no separator was appended on that path either. Whether tagging already existed in the build running then decides whether this is a migration leftover or a live gap.
2. Today at 18:16:19Z that row fired SessionEnd and the log says the row was removed, which by the BoundaryKind docs should have appended an Ended separator. The dialog shows nothing new. Ended is not clean, so it does not change the verdict here, but it looks like the same append not happening.

Practical effect while it stands: on a machine where sessions start with `claude --continue`, a row written by a pre-tagging build can never reach CLEAN until someone types /clear in it once. Five of ten local rows are in that state.
