---
name: done_memo_that_is_not_done
description: "The 2026-09-13 migration filed two open memos into done/; v1's assertion cannot catch it, so check location and not just presence when a backlog is split"
metadata: 
  node_type: memory
  type: project
---

The 2026-09-13 commit that moved this backlog to one file per memo (`486121d`) put two **open**
items into `.claude/memos/done/`. Both were reopened on 2026-09-18 and are back in the numbered
backlog; what is worth keeping is why nothing noticed for five days.

- `fix-the-homebrew-tap-casks-claude-code-dashboard-rb-on` — its line read `- [ ] 2026-09-03
  03:02` in the final state of `.claude/memos.md`, and the string `+- [x] 2026-09-03 03:02` appears
  in no revision of that file. Its body ends "nobody has run `brew install --cask
  AnotherSava/tap/claude-code-dashboard` from a clean Mac. This machine is Windows. Do that from
  AIR before checking this off."
- `terminal-tabs-windows-png-has-a-border-on-two-of-its-four` — **not** the same shape, and the
  line that used to sit here said it was. It never had a checklist line at all: reading every
  `- [` line of `git show 486121d^:.claude/memos.md` finds no match, its file was created by the
  split commit itself, and convention v10's own measurement agrees. So it reached `done/` with no
  marker to justify it — a closure nothing recorded, which is the one case v10 refuses to decide.
  **It stays open by the user's decision on 2026-09-18**, even though its body reads "DONE
  2026-09-12" with a 2026-09-13 correction: the closure was never recorded and the body's own last
  line still says "NOT VERIFIED ON WINDOWS". Do not re-raise it on the strength of that DONE.

**Convention v1's migration check asserts presence, never location.** Its Path A step 4 requires
every checklist line to appear in exactly one file under `memos/` **or** `memos/done/`, matched on
text — so an item routed to the wrong directory passes the assertion that was supposed to catch a
lost one. A repo that has adopted v1 never re-runs it, so closing that gap is the version's job
rather than this repo's. That version is v10 (`memo-split-routing`), adopted here 2026-09-18; it
carries no continuing rule, because nothing re-performs a split.

**The failure mode is silence, which is what makes it worth a memory.** Nothing resurfaces a memo
in `done/`: `memos.py list` resolves against the open backlog, and the session-start bar, task
completion and `/commit` all read the same list. A misfiled item is not wrong-looking anywhere —
it is simply absent. Whenever a backlog is split, moved or migrated again, check which side each
item landed on and not merely that it landed.

Found by cross-checking with the session working in the dotfiles repo, which reached the same two
files independently.
