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


def _is_claude_image(name: str) -> bool:
    """True if `name` is the Claude Code executable: basename, case-insensitive,
    stem (sans .exe) == 'claude'. Matches claude.exe / claude / a full path to
    either; rejects node.exe, node, etc. — a node-based install won't resolve."""
    base = os.path.basename(name.strip().replace("\\", "/")).lower()
    stem = base[:-4] if base.endswith(".exe") else base
    return stem == "claude"


def _win_proc_maps():
    """(parents pid->ppid, images pid->exe name) from one Toolhelp snapshot.
    Windows only; empty dicts on any failure. Shared by console_pids and
    agent_pid so the snapshot walk lives in one place."""
    import ctypes
    from ctypes import wintypes

    k32 = ctypes.windll.kernel32

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

    parents, images = {}, {}
    k32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
    snapshot = k32.CreateToolhelp32Snapshot(0x2, 0)  # TH32CS_SNAPPROCESS
    if snapshot and snapshot != ctypes.c_void_p(-1).value:
        entry = ProcessEntry32()
        entry.dwSize = ctypes.sizeof(ProcessEntry32)
        ok = k32.Process32First(ctypes.c_void_p(snapshot), ctypes.byref(entry))
        while ok:
            parents[entry.th32ProcessID] = entry.th32ParentProcessID
            images[entry.th32ProcessID] = entry.szExeFile.decode("ascii", "replace")
            ok = k32.Process32Next(ctypes.c_void_p(snapshot), ctypes.byref(entry))
        k32.CloseHandle(ctypes.c_void_p(snapshot))
    return parents, images


def _unix_proc_maps():
    """(parents pid->ppid, images pid->argv[0]) from one `ps` snapshot.
    macOS/Linux; empty dicts on any failure. Mirrors `_win_proc_maps` so
    `ancestors` can read the process table once per hook event."""
    parents, images = {}, {}
    try:
        out = subprocess.run(["ps", "-axo", "pid=,ppid=,comm="], capture_output=True, text=True, timeout=2).stdout
    except Exception:
        return parents, images
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
            pid = int(parts[0])
            parents[pid] = int(parts[1])
            images[pid] = parts[2] if len(parts) == 3 else ""
    return parents, images


def ancestors():
    """`(chain, agent)` from one process-table snapshot.

    `chain` is this hook's ancestor pids, nearest first, stopping at the owning
    Claude Code process (inclusive). `agent` is that process's pid — the
    `claude.exe` / `claude` image the chain stopped on — or None when the walk
    ran out first (e.g. a node-based install, whose image is node, not claude).

    Both values the widget needs come off the same walk: `agent` lets it detect
    a session that exited without a SessionEnd — which Claude Code fails to
    deliver on exit / Ctrl-D / terminal close — and remove the stranded row,
    while `chain` is where `console_pids` starts. Pure environment gathering,
    no state logic."""
    try:
        parents, images = _win_proc_maps() if os.name == "nt" else _unix_proc_maps()
        chain, pid = [], os.getpid()
        for _ in range(8):
            pid = parents.get(pid)
            if not pid or pid <= 1:
                return chain, None
            chain.append(pid)
            if _is_claude_image(images.get(pid, "")):
                return chain, pid
        return chain, None
    except Exception:
        return [], None


def console_pids(chain: list) -> list:
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

    macOS gathers only the ancestor chain: the hook's own pid is transient
    and Claude Code detaches every child it spawns from the controlling
    terminal (a hook's own `ps -o tty=` always reads `??`), but Claude Code
    itself an ancestor or two up holds the tty of the visible tab.

    `chain` from `ancestors` STOPS at the owning Claude Code process, and that
    bound is what keeps the title on the right tab. A Claude Code desktop-app
    session runs under a tty-less subtree whose own ancestors are whatever
    terminal session happened to launch it — so an unbounded walk climbs out of
    the session entirely and titles a NEIGHBOUR's tab with this agent's status,
    overwriting the status that tab should show. Bounded, a session with no tty
    of its own reports only tty-less pids and the widget correctly writes
    nothing: there is no tab to title.

    Order matters: nearest first. The widget walks far-to-near on Windows
    (transient hook-side pids hold the invisible console, so they go last;
    the bound puts claude.exe — which holds the real console — first) and
    near-to-far on macOS (dead transients and tty-less ancestors are
    skipped). Pure environment gathering, no state logic.
    """
    if os.name != "nt":
        return chain
    try:
        import ctypes

        k32 = ctypes.windll.kernel32
        buf = (ctypes.c_uint32 * 64)()
        n = k32.GetConsoleProcessList(buf, 64)
        console = [p for p in buf[: min(n, 64)] if p != os.getpid()]
    except Exception:
        console = []
    return list(dict.fromkeys(console + chain))


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
    chain, agent = ancestors()
    body = {"client": "claude", "event": event, "payload": payload, "console_pids": console_pids(chain), "agent_pid": agent}
    try:
        with urllib.request.urlopen(
            urllib.request.Request(
                url,
                data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json"},
                method="POST",
            ),
            timeout=2,
        ) as resp:
            # SessionStart is the only event whose stdout Claude Code folds back
            # into the model's context. When the instruction-adherence canary is
            # on, the widget returns an `additional_context` string (the per-session
            # marker instruction); echo it as the documented SessionStart
            # additionalContext so Claude ends each reply with the hidden marker.
            # Every other event — and a widget build / config with the feature off —
            # returns nothing here, so the hook stays silent.
            if event == "SessionStart":
                try:
                    data = json.loads(resp.read().decode("utf-8", "replace") or "{}")
                except Exception:
                    data = {}
                ctx = data.get("additional_context") if isinstance(data, dict) else None
                if ctx:
                    print(json.dumps({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ctx}}))
    except Exception:
        pass  # widget may not be running — swallow so Claude hooks don't hard-fail


if __name__ == "__main__":
    main()
