---
name: feedback_check_remote_before_fixing
description: "Before fixing a bug, check whether the repo is behind origin AND whether the installed build is behind the tree — the version number answers neither"
metadata: 
  node_type: memory
  type: feedback
---

Before diving into a fix, measure both things that go stale here. Run `git fetch` + `git log main..origin/main` + `git status -sb` for the repo, and where the symptom came from the running dashboard rather than from source, compare the installed build against the tree as well. The second axis has its own command and the version number answers neither.

**Why:** A live symptom is often a stale-binary problem rather than a missing fix, and the two causes need different commands. The repo half cost one reinvention: `374a60f` ("attach transcript watch for brand-new project dirs") was already merged, and I re-implemented it on top of stale local code when `git log main..origin/main` would have shown it. The installed-build half cost two other sessions their work on 2026-10-06 — commit `cf77f59` added `name` and `sessions` to the roster's local rows, nothing deployed for 21 hours, and two agents in other repos read a response that lacked the fields.

Comparing version numbers cannot distinguish either case. Version 1.15.0 was current on both sides of that incident with 15 code commits inside it (measured 2026-10-06), so a version match says nothing about what the running build contains.

The installed build lags the tree most of the time — 32 of 38 recoverable compile events opened a window, median 22.9h (measured 2026-10-06, an upper bound since cargo keeps only the last build per fingerprint) — so staleness on its own is the resting state and not news. What makes it bite is a commit that changes a **published shape**: a serialized field on the roster or the sync wire alters what out-of-process readers see, and they keep seeing the old shape until this machine deploys. The readers are `~/.claude/skills/shared/peer_relay.py`, the capture scripts, and the other machine's dashboard. See [[status_variant_is_a_wire_break]] for the sharper form of the same hazard, where the change breaks an older peer outright.

**How to apply:** For a bug report, run the repo check first; if behind, `git merge --ff-only origin/main` and redeploy before writing anything. For a symptom observed in the running app, compare the install's mtime against the newest commit — `INSTALL_DIR` comes from `config/deploy.env` (per-machine), and the deploy copies with no `-p`, so the binary's mtime is the install instant:

```bash
stat -f '%Sm' -t '%Y-%m-%d %H:%M:%S' "<INSTALL_DIR>/Claude Code Dashboard.app/Contents/MacOS/claude-code-dashboard"  # macOS
git log -1 --format=%cI
```

A bundle older than the newest commit touching `src-tauri/`, `src/` or `integrations/` means a deploy is owed before any diagnosis of the running app is trustworthy. The gate's wire-shape notice prints the same warning at commit time, so a response missing a field a consumer expects is the first thing to suspect rather than the last.
