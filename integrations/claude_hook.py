#!/usr/bin/env python3
"""Claude Code hook — forward lifecycle events to the Claude Code Dashboard widget.

This script is intentionally minimal: read Claude Code's event payload from
stdin, wrap it in `{client, event, payload}`, and POST to the widget's
`/api/event` endpoint. All classification, chat-id derivation, prompt
cleaning, and transcript question-detection live inside the widget's
`adapters::claude` Rust module — this file is just a transport shim.

Install in `~/.claude/settings.json`:

    {
      "hooks": {
        "SessionStart":     [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "Notification":     [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "Stop":             [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "SessionEnd":       [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "PreToolUse":       [{"matcher": "^(AskUserQuestion|ExitPlanMode)$", "hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}]
      }
    }

The `PreToolUse` matcher restricts the hook to user-gating tools whose
`tool_use` blocks aren't flushed to the JSONL transcript until the user
responds — without this hook, the dashboard cannot detect those calls in
flight. The matcher avoids the per-Bash/Read/Grep fork overhead of an
unfiltered hook.

Server URL resolution: `$TAURI_DASHBOARD_URL` if set, else `http://127.0.0.1:9077`.
"""
import json
import os
import sys
import urllib.request

DEFAULT_URL = "http://127.0.0.1:9077"


def main() -> None:
    # Claude Code sends UTF-8 JSON on stdin, but Python's default stdin
    # encoding on Windows is the system codepage (e.g. cp1251) — without this
    # line, non-ASCII chars like ⎿ become mojibake before the widget sees them.
    try:
        sys.stdin.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}
    event = payload.get("hook_event_name", "") if isinstance(payload, dict) else ""
    if not event:
        return
    url = os.environ.get("TAURI_DASHBOARD_URL", DEFAULT_URL).rstrip("/") + "/api/event"
    body = {"client": "claude", "event": event, "payload": payload}
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                url,
                data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json"},
                method="POST",
            ),
            timeout=2,
        )
    except Exception:
        pass  # widget may not be running — swallow so Claude hooks don't hard-fail


if __name__ == "__main__":
    main()
