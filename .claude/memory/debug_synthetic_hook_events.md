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

**Why:** hook-driven behavior is otherwise only observable across turns (the Stop event fires after the reply finishes), and some effects land on live user surfaces. Used 2026-06-04 to verify macOS terminal tab titles in-turn — the title write visibly landed on the user's own tab because the hook's ancestor chain from the Bash tool leads to the same `claude` process.

**How to apply:** when a change touches the hook → HTTP → state → side-effect pipeline, verify with one synthetic Set-type event + one `SessionEnd`, reading `widget.jsonl` between them. See [[debug-console-titles-tool-consoles]] for why title side effects can't be read back from the Bash tool directly.
