---
name: debug-synthetic-hook-events
description: End-to-end test dashboard features by piping synthetic events through claude_hook.py with a fake cwd; SessionEnd cleans up; verify via widget.jsonl DEBUG lines
metadata:
  type: project
---

To verify a dashboard feature end-to-end inside the current turn (no waiting for real Claude lifecycle events), pipe a synthetic payload through the real hook:

    echo '{"hook_event_name":"UserPromptSubmit","session_id":"test-0001","cwd":"/tmp/tab-title-test","prompt":"testing"}' | python3 integrations/claude_hook.py

- **Use a fake cwd** — chat_id derives from cwd, so a real project path would merge the test event into (and corrupt) the live session's row.
- Send `SessionEnd` with the same `session_id`/`cwd` afterwards to remove the row and exercise cleanup paths (blank terminal title, watcher teardown).
- Verify side effects in `widget.jsonl` (app data dir): the default tracing filter is `info,claude_code_dashboard_lib=debug`, so `tracing::debug!` lines are captured without extra config.

**Alternative — POST the wire envelope directly** (no python / env-var setup, exact control over the JSON): `curl http://127.0.0.1:9077/api/event -H 'Origin: null' -d '{"client":"claude","event":"UserPromptSubmit","payload":{"cwd":"d:/projects/zzz-test","session_id":"t1","prompt":"do a thing","transcript_path":"/tmp/t.jsonl"}}'`. Gotchas: `client` must be **`"claude"`** (the value `claude_hook.py` sends) — `"claude-code"` is silently `Ignore`d; the raw lifecycle fields nest under `payload` (the hook's envelope shape); and `Origin: null` is required to pass the CSRF guard (a browser Origin → 403). After an app restart (e.g. deploy), rows only re-materialize on the next incoming event, so re-send the flow. To simulate `/clear`: `UserPromptSubmit` → `Stop` → `SessionEnd` (marks a history separator + removes the row) → `SessionStart` (re-derives the same cwd chat_id and restores the dialog).

**Why:** hook-driven behavior is otherwise only observable across turns (the Stop event fires after the reply finishes), and some effects land on live user surfaces. Used 2026-06-04 to verify macOS terminal tab titles in-turn — the title write visibly landed on the user's own tab because the hook's ancestor chain from the Bash tool leads to the same `claude` process.

**Watcher-driven corrections need a realpath'd transcript dir (macOS).** When the e2e exercises the transcript *watcher* (append a JSONL line → `notify` → a state correction like `demote_scanned_awaiting_to_done`), the transcript file must live at a canonical path. `log_watcher` watches the parent dir and filters FSEvents by **exact** path, but macOS `/tmp` is a symlink to `/private/tmp` and FSEvents reports the canonical `/private/tmp/...` — so a `/tmp/...` transcript path never matches and the watcher **silently never fires** (the hook → state path still works, masking it). Use `os.path.realpath("/tmp")` (or any non-symlinked dir) for both `cwd` and `transcript_path`. Confirmed 2026-06-12 verifying the `Stop`-stale-read `Awaiting → Done` correction.

**How to apply:** when a change touches the hook → HTTP → state → side-effect pipeline, verify with one synthetic Set-type event + one `SessionEnd`, reading `widget.jsonl` between them. See [[debug-console-titles-tool-consoles]] for why title side effects can't be read back from the Bash tool directly.
