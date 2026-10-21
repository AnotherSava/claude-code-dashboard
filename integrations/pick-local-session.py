"""Pick a project on this Mac and open its Claude session in agterm.

The local counterpart of the remote-session picker in the claude dotfiles repo. Bound to a chord in
agterm's keymap, which runs the command line detached with no terminal; the picker is agterm's own
UI, so nothing here needs a TTY.

The dashboard starts the session (`POST /api/session/open`), not this script, so the session opens
in the agterm workspace the project's agent was last seen in and runs `claude` exactly as a session
the dashboard starts for a relayed message does. A project that already has a session is not
started twice: the dashboard answers `running`, and this script raises that session's tab instead.

A folder is listed when it is a git root that also holds a `.claude` directory, at most two levels
below the projects root. `.claude` alone, which is the remote picker's rule, also lists folders
Claude has run in that are not projects at all. A folder the scan did not reach can still be
typed, as a path below the root or an absolute one.

stderr goes nowhere a person will look, so every failure is also posted as a desktop notification.
"""

import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

PROJECTS_ROOT = os.path.expanduser(os.environ.get("PROJECTS_ROOT", "~/Projects"))
DASHBOARD = os.environ.get("TAURI_DASHBOARD_URL", "http://127.0.0.1:9077").rstrip("/")
# Kept equal by hand to COMPLETION_DEPTH and PRUNED in the claude dotfiles repo's
# remote-session/mac/pick-session.py, which scans the same depth as a `find` over ssh;
# change both, or the two chords search different layouts.
COMPLETION_DEPTH = 2
PRUNED = {".git", "node_modules", "dist", "build", "target", "__pycache__", ".venv", ".svelte-kit", ".next"}
# The dashboard waits up to its own 10s start deadline before answering.
OPEN_TIMEOUT_S = 30


def notify(body: str) -> None:
    subprocess.run(["agtermctl", "notify", body, "--title", "Local session"], capture_output=True)


def fail(message: str) -> int:
    sys.stderr.write(f"pick-local-session: {message}\n")
    notify(message)
    return 1


def agtermctl(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["agtermctl", *args], capture_output=True, text=True)


def directories() -> "list[str]":
    """Git roots below the root, at most two levels down, that also hold a `.claude` directory.

    `.git` is tested with `exists` because a worktree or a submodule carries it as a file.
    """
    found = []
    for current, children, _ in os.walk(PROJECTS_ROOT):
        depth = os.path.relpath(current, PROJECTS_ROOT).count(os.sep) + 1 if current != PROJECTS_ROOT else 0
        if depth and ".claude" in children and os.path.exists(os.path.join(current, ".git")):
            found.append(os.path.relpath(current, PROJECTS_ROOT))
        children[:] = [c for c in children if c not in PRUNED] if depth < COMPLETION_DEPTH else []
    return sorted(found)


def agent_tabs() -> "dict[str, tuple[str, str, str]]":
    """Window id, session id and workspace name per working directory with an agent tab.

    Asked of the dashboard, whose walk covers every open window and whose test for an agent is the one
    it remembers workspaces by, so the two cannot disagree about which tab holds a project's agent.
    """
    with urllib.request.urlopen(f"{DASHBOARD}/api/session/tabs", timeout=OPEN_TIMEOUT_S) as response:
        listed = json.load(response).get("tabs")
    # `null` means agterm could not be asked, which is not the same as no tabs: read as empty, every
    # project would show as not running and a running one would be blamed on another terminal.
    if listed is None:
        raise RuntimeError("could not read agterm's tabs")
    tabs = {}
    for tab in listed:
        tabs.setdefault(tab["cwd"].rstrip("/"), (tab["window"], tab["session"], tab.get("name", "")))
    return tabs


def pick(rows: "list[dict]") -> "str | None":
    result = subprocess.run(
        ["agtermctl", "pick", "open", "--prompt", "Local project", "--allow-custom"],
        input=json.dumps(rows),
        capture_output=True,
        text=True,
    )
    if result.returncode == 2:
        return None
    if result.returncode != 0:
        raise RuntimeError(f"picker failed: {result.stderr.strip() or 'unknown error'}")
    answer = json.loads(result.stdout)
    if answer.get("result") == "custom":
        return (answer.get("query") or "").strip() or None
    return answer.get("id")


def open_session(directory: str) -> dict:
    request = urllib.request.Request(
        f"{DASHBOARD}/api/session/open",
        data=json.dumps({"dir": directory}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=OPEN_TIMEOUT_S) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        return json.load(error)


def raise_tab(window: str, session: str) -> None:
    # `session select` takes its id only through --target, `window select` only positionally.
    for command in (("session", "select", "--target", session), ("window", "select", window)):
        result = agtermctl(*command)
        if result.returncode != 0:
            raise RuntimeError(f"could not bring up the tab: {result.stderr.strip()}")


def main() -> int:
    try:
        tabs = agent_tabs()
        rows = []
        for relative in directories():
            tab = tabs.get(os.path.join(PROJECTS_ROOT, relative))
            rows.append({"id": relative, "label": relative, "subtitle": f"open in {tab[2]}" if tab else "not running"})
        chosen = pick(rows)
        if chosen is None:
            return 0
        directory = os.path.normpath(os.path.join(PROJECTS_ROOT, os.path.expanduser(chosen)))
        tab = tabs.get(directory)
        if tab:
            raise_tab(tab[0], tab[1])
            return 0
        answer = open_session(directory)
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        return fail(str(error))

    if answer.get("outcome") == "running":
        try:
            tab = agent_tabs().get(directory)
            if not tab:
                return fail(f"{chosen} already has a session running, outside agterm")
            raise_tab(tab[0], tab[1])
        except (RuntimeError, ValueError, KeyError, OSError) as error:
            return fail(str(error))
        return 0
    if answer.get("outcome") != "started":
        return fail(f"could not open {chosen}: {answer.get('detail') or answer.get('reason') or 'unknown error'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
