---
name: Run deploy directly, don't hand it back
description: In this project, run `deploy` via the Bash tool after code changes instead of asking the user to run it
type: feedback
---
After making code changes that need to be visible in the running widget (frontend edits, Rust backend edits), run `deploy` via the Bash tool yourself rather than ending a turn with "run `deploy`" or "type `! deploy`".

**Why:** User asked for this on 2026-04-21 after a long iteration loop where each change ended with me asking them to run `deploy`. `deploy` is a local shell script that stops the running widget, runs `tauri build` (release), installs to `C:/Programs/tauri-dashboard`, and relaunches — all local to this machine, non-destructive, no shared/production impact. Waiting for the user to run it adds friction without any safety benefit in this setting.

**How to apply:** Once a change is code-complete and typechecks pass, call `Bash` with command `deploy` (timeout ~180 s — a release build takes 20-30 s plus frontend bundle + installer). Surface the deploy output succinctly and wait for the user to report visual feedback on the running app. Does NOT apply to commands the user hasn't pre-approved (e.g. `git push`, `tauri publish`, anything touching shared systems).
