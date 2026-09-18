---
name: done_memo_that_is_not_done
description: "The Homebrew tap memo sits in memos/done/ but its own body says the install was never verified from a Mac; two done memos carry no close date"
metadata: 
  node_type: memory
  type: project
---

`.claude/memos/done/fix-the-homebrew-tap-casks-claude-code-dashboard-rb-on.md` is filed as
addressed and is not. Its line in the retired `.claude/memos.md` was still `- [ ]` when the
2026-09-13 migration commit moved it into `done/`, which is why the convention v1 walk could
recover no close date for it, and its own body ends:

> STILL UNVERIFIED, and it is the step this memo warned about: nobody has run
> `brew install --cask AnotherSava/tap/claude-code-dashboard` from a clean Mac. This machine is
> Windows. Do that from AIR before checking this off — a green tap run is exactly what failed to
> mean anything here, and `brew audit` parses the cask without ever installing it.

Nothing will raise it again: `memos.py list` resolves against the open backlog only, so a memo in
`done/` is invisible to every surfacing point. The outstanding work is one command on the macOS
machine.

`.claude/memos/done/terminal-tabs-windows-png-has-a-border-on-two-of-its-four.md` is the other
undated file, for an unrelated reason — its `2026-09-12 00:20` stamp never appeared in any
committed `memos.md`, so no commit records when it was closed. That one is genuinely done; only
its date is unrecoverable.
