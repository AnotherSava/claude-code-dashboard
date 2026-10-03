---
name: Run deploy yourself, after asking
description: In this project run `deploy` via the Bash tool rather than handing it to the user, but ask right before each run, since it relaunches the widget
type: feedback
---
After making code changes that need to be visible in the running widget (frontend edits, Rust backend edits), run `deploy` via the Bash tool yourself rather than ending a turn with "run `deploy`" or "type `! deploy`". **Ask immediately before each run**, though: deploy stops and relaunches the widget, and a relaunch is a launch that can take focus from what the user is doing.

**Why:** On 2026-04-21 the user asked for deploys to be run, not handed back, after a long loop where each change ended with "run `deploy`". On 2026-10-02 they added that starting or restarting an app counts as taking over the machine, because it changes focus from what they are using at that moment. That day a workflow subagent had deployed unasked, and a sibling session had restarted the user's terminal twice to install a build.

**How to apply:** Once a change is code-complete and the gate passes, say that you are about to deploy (relaunching the widget) and wait for a yes; then call `bash scripts/deploy.sh` (timeout ~10 min). One yes covers one deploy. Never let a subagent or workflow stage deploy: put an explicit "never deploy, start, restart or stop any app" line in its prompt. The same applies to restarting any other app the user may be in, such as agwinterm or a terminal. Does NOT authorize anything touching shared systems (`git push`, `tauri publish`).
