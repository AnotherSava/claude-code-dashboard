#!/usr/bin/env python3
"""macOS figure `dev-server-line-macos`: the green dev-server line under a session's agterm sidebar row.

THE STATE IS STAGED THROUGH THE PRODUCT'S OWN DOOR. A throwaway `http.server` is
started on this project's own registered Vite port (1420, `tauri-dashboard-vite-dev`
in the ports registry) and announced through `POST /api/dev-server`, the request
the deploy skill's `deploy-dev-server.sh` makes. The dashboard's worker then probes
the port and writes agterm's `session info` line itself, so the pixels are the
production path rendering a state real use reaches. Nothing is written to agterm
by this script; the server is stopped on the way out, after which the dashboard
forgets it on its next probe and clears the line.

THE SESSION is the one this script runs in (`$AGTERM_SESSION_ID`) unless
`--session` names another. Its row carries the title the dashboard wrote for it,
so its project has to be public, and so does every row and workspace header
above it: the frame runs from the top of agterm's window down to the line, which
keeps the window's own rounded top-left corner and traffic lights in frame and
cuts the right and bottom sides. Every name in it is vetted against
`--publishable`, workspace headers included, before anything is captured.

THE CROP is measured, not hardcoded. Its width is agterm's `sidebarWidth` from the
tree, in logical points, times the capture's scale. Its bottom is the info line
itself, found as the one band of agterm's green that sits right of the status
glyph column, then carried down past the row's card to the sidebar's own
background, so the frame ends on the gap before the next row rather than on the
card's edge. A second band (another session's dev server) refuses the capture,
since the pixels cannot say which is this session's and a lower one would pull
unvetted rows into the frame.

agterm has to be the active application, as it is whenever this runs from
inside it with the terminal in front: an inactive window draws its traffic
lights grey, and `check-figures.py` recognises a macOS frame by their colour.
The crop is put through that gate's own `has_mac_chrome` before it is written.

The widget is hidden for the shot, since it is always-on-top, and put back
afterwards including on a failed capture.
"""
import argparse
import http.server
import importlib.util
import json
import os
import socket
import sys
import threading
import time
from pathlib import Path

from PIL import Image

# The lib sits beside this file; it is imported after the path insert, which is
# the one import that cannot live with the others.
sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
import dashboard as dash  # noqa: E402

# The figure gate, loaded by path since its file name is not an identifier, so
# the crop is judged by the very test it has to pass.
_gate_spec = importlib.util.spec_from_file_location("check_figures", dash.SHOTS / "check-figures.py")
check_figures = importlib.util.module_from_spec(_gate_spec)
_gate_spec.loader.exec_module(check_figures)

FIGURE = "dev-server-line-macos"
PORT = 1420                 # this project's own Vite dev port, from the ports registry
LINE_WAIT_S = 10            # how long the dashboard gets to write the line after the announce
GAP_PT = 3                  # kept below the row's card, half the gap to the next row
GLYPH_COLUMN_PT = 58        # left of this is the status glyph column, whose 🟢 is green too: measured on 0.40.0, the glyph ends at 55pt and the line's ▶ starts at 60pt
TITLE_BAR_PT = 28           # above this is the title bar, whose green traffic light reaches past GLYPH_COLUMN_PT: measured on 0.40.0, it spans 9-23pt down and 55-68pt across


def locate(session_id: str) -> tuple[dict, dict, list[str], list[str]]:
    """The window holding the session, its tree, and the workspace headers and session labels drawn from the top of the sidebar through that session's row."""
    for window in dash.agtermctl("window", "list").get("windows", []):
        if window.get("open") is False:
            continue
        tree = dash.agtermctl("tree", "--window", window["id"]).get("tree", {})
        headers, rows = dash.sidebar_rows(tree)
        for i, (workspace, session) in enumerate(rows):
            if session.get("id") == session_id:
                shown = headers[:next(k for k, h in enumerate(headers) if h is workspace) + 1]
                return window, tree, [h.get("name", "") for h in shown], [dash.sidebar_label(s) for _, s in rows[:i + 1]]
    raise dash.CaptureError(f"No open agterm window draws session {session_id} in its sidebar: its workspace is collapsed, or hidden by agterm's workspace filter.")


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass


def serve(port: int) -> http.server.ThreadingHTTPServer:
    """The throwaway server the line names. Refuses a port already in use rather than announcing someone else's server."""
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", port)) == 0:
            raise dash.CaptureError(f"Port {port} already answers, so announcing it would put somebody else's server in the figure. Stop it, or pass --port.")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Quiet)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def announce(port: int, session: str) -> None:
    dash.post("/api/dev-server", {"port": port, "dir": str(dash.REPO), "session": session}, what="announce")


def wait_for_line(window_id: str, session_id: str, port: int) -> None:
    """Wait until agterm reports the session's info line naming the port, so the shot is never taken before the write."""
    deadline = time.monotonic() + LINE_WAIT_S
    while time.monotonic() < deadline:
        for workspace in dash.agtermctl("tree", "--window", window_id).get("tree", {}).get("workspaces", []):
            for session in workspace.get("sessions", []):
                if session.get("id") == session_id and f":{port}" in json.dumps(session, ensure_ascii=False):
                    return
        time.sleep(0.5)
    raise dash.CaptureError(f"agterm showed no line naming :{port} on session {session_id} within {LINE_WAIT_S}s. Is the deployed dashboard new enough to serve /api/dev-server?")


def is_line_green(r: int, g: int, b: int, a: int) -> bool:
    return a > 200 and g > 120 and g - max(r, b) > 50


def line_bottom(im: Image.Image, top: int, left: int, right: int) -> int:
    """The bottom of the one band of green below the title bar and right of the glyph column, which is the session's info line.

    Refuses more than one band: another session's line can sit anywhere in the
    sidebar, and the pixels cannot say which band is this session's.
    """
    rows = [y for y in range(top, im.height) if any(is_line_green(*im.getpixel((x, y))) for x in range(left, right))]
    if not rows:
        raise dash.CaptureError("No green dev-server marker was found in the sidebar. Nothing was written.")
    bands = 1 + sum(1 for above, y in zip(rows, rows[1:]) if y - above > 1)
    if bands > 1:
        raise dash.CaptureError(f"The sidebar shows {bands} bands of agterm's green right of the glyph column, so another session's line (or other green text) is on screen and the crop cannot tell which is this session's. Stop the other dev servers and run again. Nothing was written.")
    return max(rows)


def card_bottom(im: Image.Image, below: int, x: int) -> int:
    """The first row under the line where column `x` is back on the sidebar's background, the colour at the window's left edge."""
    bg = im.getpixel((2, below))
    for y in range(below, im.height):
        if max(abs(a - b) for a, b in zip(im.getpixel((x, y))[:3], bg[:3])) <= 4:
            return y
    raise dash.CaptureError("The row's card runs to the bottom of the window. Nothing was written.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("--session", default=os.environ.get("AGTERM_SESSION_ID"), help="the agterm session whose row gets the line (default: the one this runs in)")
    parser.add_argument("--port", type=int, default=PORT, help=f"the port the throwaway server takes (default {PORT}, this project's Vite port)")
    parser.add_argument("--publishable", nargs="+", default=list(dash.PUBLISHABLE_PROJECTS), metavar="NAME", help="every name allowed above the line, workspace headers included")
    parser.add_argument("--probe", action="store_true", help="write to tmp/ instead of the committed frame")
    args = parser.parse_args()
    if not args.session:
        print("refused: not inside an agterm session; pass --session", file=sys.stderr)
        return 1
    try:
        return capture(args)
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        return 1


def capture(args: argparse.Namespace) -> int:
    window, tree, headers, labels = locate(args.session)
    dash.require_sidebar_drawn(tree, window["id"])
    private = dash.private_sidebar_names(headers, labels, args.publishable)
    if private:
        raise dash.CaptureError(f"{len(private)} name(s) above the line are not on --publishable: {', '.join(repr(p) for p in private)}. Every one is published verbatim.")
    width_pt = float(tree["sidebarWidth"])

    server = serve(args.port)
    try:
        announce(args.port, args.session)
        wait_for_line(window["id"], args.session, args.port)
        time.sleep(0.5)
        selected = dash.selected_label(tree)
        raw = dash.probe_path(f"{FIGURE}-raw")
        with dash.widget_hidden():
            win = dash.find_window(dash.AGTERM, title=selected)
            dash.capture_window(win, raw)
    finally:
        server.shutdown()

    with Image.open(raw) as opened:
        im = opened.convert("RGBA")
        icc = opened.info.get("icc_profile")
    scale = round(im.width / win["w"])
    right = min(im.width, round(width_pt * scale))
    bottom = min(im.height, card_bottom(im, line_bottom(im, TITLE_BAR_PT * scale, GLYPH_COLUMN_PT * scale, right), right // 2) + GAP_PT * scale)
    frame = im.crop((0, 0, right, bottom))
    staged = dash.probe_path(f"{FIGURE}-crop")
    frame.save(staged, icc_profile=icc)
    if not check_figures.has_mac_chrome(staged):
        # The red light, 16pt in and down at either scale: grey there means
        # agterm's window was not key when DocShot took it.
        r, g, b, _ = frame.getpixel((round(16 * scale), round(16 * scale)))
        raise dash.CaptureError(f"The crop fails check-figures.py's macOS-chrome test (red light reads ({r}, {g}, {b}); grey means agterm's window was not the active one). Bring agterm to the front and run again. Nothing was written.")
    out = dash.probe_path(FIGURE) if args.probe else dash.shot_path(FIGURE)
    frame.save(out, icc_profile=icc)
    # The window's own edge survives on the top and left only, so the frame gets
    # a hairline traced from its alpha; `add_hairline` keeps the raw beside the
    # committed frame first and re-tags the result sRGB.
    if not args.probe:
        dash.add_hairline(out)
    print(f"{out.relative_to(dash.REPO)}  {right}x{bottom} at {scale}x; names in frame: {', '.join(headers + labels)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
