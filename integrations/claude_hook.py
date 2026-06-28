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
        "SessionStart":        [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "UserPromptSubmit":    [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "UserPromptExpansion": [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "Notification":        [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "Stop":                [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "StopFailure":         [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "PermissionRequest":   [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "Elicitation":         [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "ElicitationResult":   [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "PreCompact":          [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "SessionEnd":          [{"hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}],
        "PreToolUse":          [{"matcher": "^(AskUserQuestion|ExitPlanMode)$", "hooks": [{"type": "command", "command": "python <repo>/integrations/claude_hook.py"}]}]
      }
    }

`StopFailure` (turn ended on an API error → ERROR), `PermissionRequest` and
`Elicitation` (blocked on the user → WAIT), `ElicitationResult` (the user
answered the MCP prompt → resume Working), and `PreCompact` (context
compaction → a history separator) cover gaps the core lifecycle events leave.

The `PreToolUse` matcher restricts the hook to user-gating tools whose
`tool_use` blocks aren't flushed to the JSONL transcript until the user
responds — without this hook, the dashboard cannot detect those calls in
flight. The matcher avoids the per-Bash/Read/Grep fork overhead of an
unfiltered hook.

`UserPromptExpansion` fires the instant a slash command is invoked — seconds
before `UserPromptSubmit`, which Claude Code only emits after the command's
context-gathering completes — so a skill launch flips the row to Working at
once instead of lingering on the prior state.

Server URL resolution: `$TAURI_DASHBOARD_URL` if set, else `http://127.0.0.1:9077`.
"""
import json
import os
import subprocess
import sys
import urllib.request

DEFAULT_URL = "http://127.0.0.1:9077"


def console_pids() -> list:
    """Candidate pids for the widget's terminal-tab-title writes.

    The widget sets a session's tab title through one of these pids — on
    Windows by attaching to its console (AttachConsole + SetConsoleTitleW),
    on macOS by resolving its controlling tty and writing an OSC escape —
    see the widget's `terminal_title` module.

    Windows gathers two sources, because Claude Code spawns hooks with
    CREATE_NO_WINDOW, which gives the hook a fresh *invisible* console
    rather than the terminal's:

    - processes attached to this hook's own console — only useful in setups
      where the hook inherits the real console;
    - this process's ancestor chain — the long-lived Claude Code process an
      ancestor or two up holds the visible terminal console.

    macOS gathers only the ancestor chain (one `ps` snapshot): the hook's
    own pid is transient, but Claude Code an ancestor or two up shares the
    controlling tty of the visible tab.

    Order matters: nearest first. The widget walks far-to-near on Windows
    (transient hook-side pids hold the invisible console, so they go last)
    and near-to-far on macOS (dead transients and tty-less GUI ancestors
    are skipped). Pure environment gathering, no state logic.
    """
    if os.name != "nt":
        # macOS/Linux: ancestor pid chain, nearest first.
        try:
            out = subprocess.run(["ps", "-axo", "pid=,ppid="], capture_output=True, text=True, timeout=2).stdout
            parents = {}
            for line in out.splitlines():
                parts = line.split()
                if len(parts) == 2 and parts[0].isdigit() and parts[1].isdigit():
                    parents[int(parts[0])] = int(parts[1])
            pids = []
            pid = os.getpid()
            for _ in range(6):
                pid = parents.get(pid)
                if not pid or pid <= 1:
                    break
                pids.append(pid)
            return pids
        except Exception:
            return []
    try:
        import ctypes
        from ctypes import wintypes

        k32 = ctypes.windll.kernel32
        me = os.getpid()

        buf = (ctypes.c_uint32 * 64)()
        n = k32.GetConsoleProcessList(buf, 64)
        pids = [p for p in buf[: min(n, 64)] if p != me]

        # Ancestor chain via a Toolhelp snapshot (stdlib-only pid→ppid map).
        class ProcessEntry32(ctypes.Structure):
            _fields_ = [
                ("dwSize", wintypes.DWORD),
                ("cntUsage", wintypes.DWORD),
                ("th32ProcessID", wintypes.DWORD),
                ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
                ("th32ModuleID", wintypes.DWORD),
                ("cntThreads", wintypes.DWORD),
                ("th32ParentProcessID", wintypes.DWORD),
                ("pcPriClassBase", ctypes.c_long),
                ("dwFlags", wintypes.DWORD),
                ("szExeFile", ctypes.c_char * 260),
            ]

        k32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
        snapshot = k32.CreateToolhelp32Snapshot(0x2, 0)  # TH32CS_SNAPPROCESS
        if snapshot and snapshot != ctypes.c_void_p(-1).value:
            entry = ProcessEntry32()
            entry.dwSize = ctypes.sizeof(ProcessEntry32)
            parents = {}
            ok = k32.Process32First(ctypes.c_void_p(snapshot), ctypes.byref(entry))
            while ok:
                parents[entry.th32ProcessID] = entry.th32ParentProcessID
                ok = k32.Process32Next(ctypes.c_void_p(snapshot), ctypes.byref(entry))
            k32.CloseHandle(ctypes.c_void_p(snapshot))
            pid = me
            for _ in range(6):
                pid = parents.get(pid)
                if not pid:
                    break
                pids.append(pid)

        return list(dict.fromkeys(pids))
    except Exception:
        return []


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
    body = {"client": "claude", "event": event, "payload": payload, "console_pids": console_pids()}
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
