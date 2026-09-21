---
name: memo_backlog_audit_2026_09_20
description: The whole memo backlog was audited for already-done items on 2026-09-20; one closed, the rest verified genuinely open
metadata:
  type: project
---

A memo can be finished and left open, and its own body is where that shows. The Homebrew tap memo carried `DONE 2026-09-11 from AIR` at the end of a long body and still sat in `.claude/memos/`, because closing is a separate command nobody ran. Read the body before checking the repo — it is the cheapest signal by a wide margin, and it often reads `DONE EXCEPT <x>`, which means partial rather than done.

**The rest of the backlog was audited on 2026-09-20 and nothing else was closable.** Every remaining memo got an investigator, and every done-or-partial verdict was then attacked by three independent refuters; none survived. Three looked finished and were not — the `terminal-tabs-windows.png` border memo (the image is correct, the re-shoot path was broken), the Windows capture scripts memo (one of five scripts has ever run), and the docs-screenshots rework (two frames never re-shot). Do not re-run this sweep on a hunch: it cost ~3.7M subagent tokens, and the backlog only gains memos, so a repeat finds the same answer for everything above the date.

One thing the sweep turned up that no memo recorded: the capture libs resolved a dotfiles skill path that had been renamed out from under them, fixed the same day and now gated by `docs/screenshots/check-skill-scripts.py`.

**Its second "finding" was mostly a misreading, and the misreading is the durable part.** The sweep reported that two committed macOS frames publish private project names and that `PUBLISHABLE_PROJECTS` wrongly lists `what-is-next` as public. Both the constant and the capture script carry long comments saying otherwise — the list is named *publishable* rather than *public* because that entry is private and cleared by its owner, and `terminal-tabs-macos.py`'s docstring already records that the committed frame predates its own guard. Every agent involved grepped the constant and read downward to its consumer; none read the lines above it. Only the residue survived as a memo. See [[feedback_read_upward_from_the_match]] — this is that failure at scale, and a confident wrong claim reached the user before the comment did.

Related: [[done_memo_that_is_not_done]], which is the opposite error — a memo filed as done while its work was outstanding.
