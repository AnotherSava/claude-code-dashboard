---
name: claude_md_single_line_merge
description: CLAUDE.md's module map is one ~75k-char line, so edits from both machines always conflict; never resolve the markers — re-apply the edits onto the pulled text
metadata:
  type: project
---

The "Key module map" paragraph in `CLAUDE.md` is a **single line of ~75,000 characters**. Git diffs by line, so two edits to entirely unrelated sentences are the same hunk and conflict every time. Since this repo is worked from both a Windows and a macOS machine and nearly every feature updates that paragraph, a conflicting `git pull` on `CLAUDE.md` is the normal case, not an incident.

**Why:** On 2026-08-30 a local edit (recording the Windows relay leg as verified) collided with an incoming commit that had inserted new roster and reply-path text elsewhere in the same line. The conflict markers wrapped the whole 75k-character line twice — nothing hand-resolvable. Both sides' changes were small and disjoint; only the line granularity made them collide.

**How to apply:** Don't `git stash pop` and don't edit conflict markers. Stash the file, `git merge --ff-only origin/main`, then re-apply each local change as an exact-string `Edit` against the *pulled* text, and drop the stash. Confirm first that upstream still contains each original sentence verbatim (`git show origin/main:CLAUDE.md | grep -c "<old sentence>"` → 1); a 0 means upstream revised that same passage and the change needs merging by judgment instead. Verify the result by reconstructing "upstream + exactly my edits" and asserting byte-equality, or by blanking the edited regions in both texts and comparing skeletons — a character-level differ over that line produces dozens of meaningless fragments and proves nothing. Full mechanics in the global learning `git-stash-pull-safety.md`. See also [[feedback_check_remote_before_fixing]].
