---
name: benign_openers_offer_questions
description: benign_openers config neutralizes "Anything …?" offer questions; symmetric with benign_closers, don't unify away
metadata:
  type: project
---

`Config::benign_openers` (default `["anything"]`) marks a question whose **final
sentence** opens with one of these words as an optional *offer* rather than a
hand-back — so a sign-off like "Anything you'd like to look at?" stays DONE
instead of flipping the row to WAIT. It's prefix-matched on the last sentence,
mirroring `benign_closers` (suffix-matched). Both are bundled into the borrowed
`QuestionRules { closers, openers }` struct threaded through
`adapters/claude.rs::question_reason` and `log_watcher::flushed_turn_verdict`.

**Why:** "Anything …?" sign-offs were tripping the bare "ends with `?`" path. Every
such phrase in `prompt_history.json` is a pure offer, never a real ask.

**How to apply:** The opener check only skips the bare-`?` path — an embedded real
ask still fires via the permission-seeking path, so "Anything else, or shall I
commit?" still WAITs (caught by "shall i"). Closers diverge here on purpose: a
benign **closer** (default `["What's next?", "or are you good?", "or leave it?", "or leave it parked?", "or leave that to you?", "or are you set to check it yourself?"]`) excuses its
*whole closing sentence* (it's dropped before the permission-seeking / hand-back
paths run), so "Want me to drive a browser check, or are you good?" stays DONE
even though it opens with "Want me to". A real ask in an *earlier* sentence still
WAITs ("Should I delete the backup first? Or are you good?"). Don't try to
"simplify" openers and closers into one list or re-derive on the frontend —
they're deliberately distinct (prefix-skips-Path-1 vs suffix-excuses-its-sentence),
like the [[feedback_frontend_question_detector_lenient]] split. Validate any change to this against `prompt_history.json` per
[[feedback_validate_detection_against_history]].
