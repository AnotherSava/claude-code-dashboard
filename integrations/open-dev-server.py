"""Open the newest dev server this agterm session shows as an overlay over it, or hide and show it.

Bound to a chord in agterm's keymap, which runs the command line detached with no terminal and
exports the session the chord fired in as `AGT_SESSION_ID`. The dashboard knows which servers were
started from that session, and which the other machine's row it renders as a `⇄` tab runs, because
the deploy skill's `deploy-dev-server.sh` announces each one (`POST /api/dev-server`), on that
machine for a peer's; this asks it (`GET /api/dev-server`) and opens the newest.

The page opens at the URL the dashboard gives: `localhost` for a server started on this Mac, and
for one the other machine's session started — this being its `⇄` tab — the tailnet address that
machine's deploy published, since `localhost` would name this Mac. It opens with its JavaScript on
(an overlay runs none by default, and a dev server is an app), the navigation buttons, and the
shared persistent browser store, so a login survives the overlay closing.

Pressed again, the chord toggles the session's page overlay instead of opening a second one, which
agterm refuses while one is open or hidden. Any page overlay counts, not only a dev server's: with
one already there, agterm would refuse the open anyway.

stderr goes nowhere a person will look, so a failure is shown in a panel over the session that
closes by itself. A desktop notification would leave an unread badge on the session for every press.
"""

import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

DASHBOARD = os.environ.get("TAURI_DASHBOARD_URL", "http://127.0.0.1:9077").rstrip("/")
HUD_SECONDS = "4"


def _fail(message: str, session: str) -> int:
    sys.stderr.write(f"open-dev-server: {message}\n")
    if session:
        subprocess.run(["agtermctl", "session", "hud", "open", message, "--hide-after", HUD_SECONDS, "--target", session], capture_output=True)
    return 1


def _agtermctl_json(*args: str) -> dict:
    """agtermctl's JSON answer, or a refusal in the same shape when it gave none. agterm refuses
    with `{"ok":false,"error":…}`, sometimes with exit 0, so the envelope is what is read."""
    done = subprocess.run(["agtermctl", *args, "--json"], capture_output=True, text=True)
    try:
        return json.loads(done.stdout)
    except ValueError:
        return {"ok": False, "error": (done.stderr or done.stdout).strip()}


def _has_page_overlay(session: str) -> bool:
    """Whether the session holds a page overlay, shown or hidden. The chord fires in the frontmost
    window, which is the one a bare `tree` describes."""
    def find(node):
        if isinstance(node, dict):
            if node.get("id") == session:
                return node
            node = list(node.values())
        if isinstance(node, list):
            for child in node:
                hit = find(child)
                if hit is not None:
                    return hit
        return None

    found = find(_agtermctl_json("tree").get("result"))
    return bool(found and found.get("htmlOverlays"))


def _servers(session: str) -> "list[dict]":
    query = urllib.parse.urlencode({"session": session})
    with urllib.request.urlopen(f"{DASHBOARD}/api/dev-server?{query}", timeout=5) as resp:
        return json.load(resp).get("servers") or []


def main() -> int:
    session = os.environ.get("AGT_SESSION_ID", "")
    if not session:
        return _fail("no session to open a dev server for", session)
    if _has_page_overlay(session):
        answer = _agtermctl_json("session", "overlay", "toggle", "--target", session)
        return 0 if answer.get("ok") else _fail(f"agterm would not toggle the overlay: {answer.get('error')}", session)
    try:
        servers = _servers(session)
    except (urllib.error.URLError, OSError, ValueError) as e:
        return _fail(f"the dashboard did not answer: {e}", session)
    if not servers:
        return _fail("no dev server is running from this session", session)
    newest = servers[0]
    url = newest.get("open")
    if not url:
        return _fail(f"the dev server on {newest.get('device')} (port {newest['port']}) has no tailnet address to open it from here", session)
    answer = _agtermctl_json("session", "overlay", "open", "--url", url, "--js", "--navigation", "--persistent", "--target", session)
    return 0 if answer.get("ok") else _fail(f"agterm would not open {url}: {answer.get('error')}", session)


if __name__ == "__main__":
    sys.exit(main())
