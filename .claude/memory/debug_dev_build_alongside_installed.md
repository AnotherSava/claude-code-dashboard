---
name: debug-dev-build-alongside-installed
description: Quit the installed dashboard before `npm run tauri dev` — two instances collide on 9077/9078 and the dev one blanks the live one's terminal tab titles; how to prove the webview loaded, and the teardown pattern that misses the binary
metadata:
  type: project
---

A `tauri dev` run and the installed app cannot both be up. They share `~/Library/Application Support/com.anothersava.claude-code-dashboard/`, and the second instance loses the bind on both 9077 and 9078. The damage that is not obvious is `terminal_title::sync`, which blanks the title of any console or tty whose row it does not hold — so a dev instance that has seen no hook events wipes the status glyphs the live instance wrote across every terminal tab, and the live one only rewrites them on its next state change.

So: quit the installed app, run dev, relaunch. `osascript -e 'quit app "Claude Code Dashboard"'` exits cleanly; confirm with `lsof -nP -iTCP:9077 -sTCP:LISTEN` before launching. Ask the user first — quitting costs the live instance everything in memory, including `attended_at` stamps, any open subagent prompt gate, and outstanding Telegram alerts it can no longer revoke, since `context_outstanding` is in-memory.

**The teardown pattern that looks right and misses.** Cargo exec's the dev binary with a *relative* argv, `target/debug/claude-code-dashboard`, so a `pkill -f` on the absolute path matches the Tauri CLI and Vite and leaves the app itself holding 9077. Kill by pid from `lsof -nP -iTCP:9077 -sTCP:LISTEN -t`, and never widen the pattern to a bare `claude-code-dashboard`, which also matches the installed app. Check the port rather than trusting the sweep.

**Proving the webview actually loaded, without a screenshot.** `FrontendLogger` writes IPC log entries into `widget.jsonl`, so frontend-originated lines are the evidence: `mount snapshot`, `auto_resize measure` and `auto_resize::apply` appear only if the Svelte app booted and reached the backend. `sessions_updated` with a session count confirms events are flowing the other way. A `curl 127.0.0.1:9077/api/agents` proves only the backend.

**Relaunching does not restore the peers.** `AppState::remote` is in-memory and refills only when a peer pushes, so the roster comes back empty and fills per device on its own heartbeat — measured 2026-10-06, one peer returned in 10s while a second had not come back after 80s, which is that machine's own cadence and not something the restart can fix.

Distinct from [[debug_live_widget_testing]], which covers iterating on the UI of an already-running instance; this is about standing a second one up at all.
