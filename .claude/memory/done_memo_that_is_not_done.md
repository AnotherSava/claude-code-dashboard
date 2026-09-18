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
- `terminal-tabs-windows-png-has-a-border-on-two-of-its-four` — same shape. Its body says "NOT
  FIXED FROM HERE on purpose" and names the remaining work: port `add_hairline` and `has_own_edge`
  to the Windows capture lib and call them from the terminal-tabs script.

**Convention v1's migration check asserts presence, never location.** Its Path A step 4 requires
every checklist line to appear in exactly one file under `memos/` **or** `memos/done/`, matched on
text — so an item routed to the wrong directory passes the assertion that was supposed to catch a
lost one. A repo that has adopted v1 never re-runs it, so closing that gap is the version's job
rather than this repo's.

**The failure mode is silence, which is what makes it worth a memory.** Nothing resurfaces a memo
in `done/`: `memos.py list` resolves against the open backlog, and the session-start bar, task
completion and `/commit` all read the same list. A misfiled item is not wrong-looking anywhere —
it is simply absent. Whenever a backlog is split, moved or migrated again, check which side each
item landed on and not merely that it landed.

Found by cross-checking with the session working in the dotfiles repo, which reached the same two
files independently.
