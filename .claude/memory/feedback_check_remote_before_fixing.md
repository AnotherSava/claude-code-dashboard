---
name: feedback_check_remote_before_fixing
description: "Before fixing a bug, check git log + remote — the deployed app often lags origin/main and the fix may already exist"
metadata: 
  node_type: memory
  type: feedback
---

When a reported bug "feels" like it could already be handled, run `git fetch` + `git log main..origin/main` (and `git status -sb`) BEFORE diving into a fix.

**Why:** This repo's deployed build routinely lags `origin/main` (e.g. running 0.7.0 while remote was at 0.8.2, 9 commits ahead). A live symptom can be a stale-binary problem, not a missing fix. I re-implemented an already-merged fix (`374a60f` "attach transcript watch for brand-new project dirs" — the SessionStart-races-transcript-dir-creation watcher bug that strands a session on `Awaiting`) on top of stale local code instead of just pulling. The remote fix (`ensure_watch_dir` pre-creates the dir) was simpler than my reinvention.

**How to apply:** For any bug report, first reconcile deployed version vs `origin/main`. If behind, `git merge --ff-only origin/main` and redeploy before writing new code — the fix may already be upstream. Only write a new fix once you've confirmed it isn't.
