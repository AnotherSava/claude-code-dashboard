#!/usr/bin/env python3
"""macOS variant of the `terminal-tabs` figure.

agterm's session sidebar, each row carrying the status glyph this dashboard wrote
onto that session's tab title, two of them with a ` [N%]` context-usage suffix.
The Windows half of the figure is Windows Terminal's tab strip: a
different application showing the same information, not this picture on another
OS, which is why the docs caption each variant with the application it shows.

WHAT THE OPERATOR STAGES BY HAND, because this script reads agterm and never
writes to it. A capture script that created, selected or resized sessions would
photograph a sidebar the user never had:

  * The sessions, in public repositories only. A row's label is the dashboard's
    own tab title, i.e. the project name, and the workspace headers are in frame
    too — so `--publishable` lists every name allowed to appear, headers included,
    and anything else refuses the capture. It defaults to `dash.PUBLISHABLE_PROJECTS`,
    the names checked against GitHub as public repositories, which holds no
    workspace names at all, so this figure normally has to pass the flag.
  * A spread of glyphs — the committed frame carries four of the six (⚪ idle,
    🔵 working, 🟢 done, ✋ blocked; the other two are ⏳ waiting and 🔴 error) —
    and the two sessions past `terminal_title_context_percent` that put a ` [N%]`
    on a row, which is the count promised by both `screenshots.json` and the alt
    text in `docs/pages/features.md`. The percentage comes from the transcript
    watcher, so it cannot be faked through the hook API: the session has to
    really be that far into its context window.
  * agterm's window at the sidebar's own size. See below.

There is deliberately no `--force` for those content checks. A frame with one
glyph and no percentage is not this figure, and it is the kind of wrong picture
that looks perfectly plausible on the page.

The committed frame predates this script and passed none of those gates: it was
taken by hand, and its three workspace headers and ten session rows carry names
that `dash.PUBLISHABLE_PROJECTS` does not vouch for. Re-shooting that same sidebar
means naming every one of them on `--publishable`, which is worth doing only for the
ones whose repositories really are public.

THE FRAME IS THE SIDEBAR AND NOTHING ELSE, and there are two ways to get there.
The committed frame took the first: the whole window, with agterm's window sized
down to its sidebar so the terminal surface is not in it at all. That is readable
off the pixels — the light border runs down both edges and all four corners are
rounded — and it is the better picture, because the frame then ends on agterm's
own border and rounded right corner rather than on a cut. The second way is to
crop a full-size window down to the sidebar, which leaves the two right corners
square where its sibling's are round. So the script measures, refuses the cut by
default, and takes it under `--allow-cut`.

MEASURED, NOT HARDCODED, for the same reason the Windows script measures both of
its crop bounds: agterm publishes `sidebarWidth` in its own per-window state file
under `~/Library/Application Support/agterm/windows/<window-id>.json`, in the
logical points `window list`'s geometry uses, and the capture's own scale
(captured pixels over the window's logical width) converts it to device pixels.
That file is private, undocumented state with no compatibility promise — the
dashboard's own reader in `src-tauri/src/terminals/agterm.rs` checks its
`version` for exactly that reason — so an unrecognized version refuses here too
and names `--sidebar-width` as the way past it, rather than trusting a number out
of a layout nobody has read. The bottom bound is measured off the pixels: the
last row of the sidebar that differs from its flat background, which is where the
session rows end.

The sanity check on all of it is the frame being replaced: 554x808 device pixels,
277x404 logical at 2x. The sidebar measured 275.9 logical points against that
277, the difference being the window's own border, which is what `EDGE_SLACK_PT`
exists to absorb.

The widget is hidden for the shot. It is always-on-top, so raising agterm does
not get it out from under, and the lib puts it back afterwards including on a
failed capture.

The compositor's drop shadow is trimmed by `lib/trim_halo.py` and the trimmed
capture is then converted to sRGB with `sips`. The blue alpha-9 bleed pixel this
figure's top-left corner used to carry was that shadow; the conversion is there
because `screencapture` tags what it writes with the display's own wide-gamut
profile, which leaves every stored pixel wrong for anything that reads the
numbers back — `check-figures.py` identifies a macOS frame by the saturation of
a window's traffic lights, and that is the check the wide-gamut tag broke. Both
steps run inside `dash.capture_window`, the conversion last. Nothing here
repeats either.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

from PIL import Image

# The lib sits beside this file; it is imported after the path insert, which is
# the one import that cannot live with the others.
sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
import dashboard as dash  # noqa: E402

FIGURE = "terminal-tabs-macos"
AGTERM = "Agterm"          # the CGWindowOwnerName of agterm's windows
AGTERM_STATE = Path.home() / "Library" / "Application Support" / "agterm" / "windows"
SNAPSHOT_VERSION = 1       # the only layout this script knows how to read

# The map `status_glyph` writes, from `src-tauri/src/terminal_title.rs`, whose
# inverse `parse_title` lives beside it so the two halves cannot drift. A leading
# token that is not one of these is a title this dashboard did not write.
GLYPHS = {"⚪": "idle", "🔵": "working", "⏳": "waiting", "✋": "blocked", "🟢": "done", "🔴": "error"}
CONTEXT_SUFFIX = re.compile(r"\[(\d+)%\]")

COMMITTED_PT = (277, 404)  # the frame being replaced, in logical points

# All in logical points, converted by the capture's own scale. Measured on the
# committed frame: its border is 1pt, its session rows are 28pt apart, and its
# last row sits about 12pt above the window's bottom edge.
EDGE_SLACK_PT = 4          # window wider than its sidebar by less than this = sidebar-sized
TAIL_SLACK_PT = 28         # one empty session row of dead space at the bottom is tolerated
BOTTOM_PAD_PT = 12         # kept below the last row when a crop does have to cut
INSET_PT = 24              # clear of the rounded corners and the border when scanning rows
BORDER_PT = 4              # the window's own bottom edge, excluded from the row scan
PLAUSIBLE_PT = (120, 480)  # a sidebar width outside this is a broken measurement, not a drag


# ------------------------------------------------------------------ agterm, read

def agtermctl(*args: str) -> dict:
    """Run one READ-ONLY agtermctl command and return its parsed answer.

    Every call this script makes is a query — `window list` and `tree`. It never
    creates, selects, closes, resizes or types, because the staging is the
    operator's and a rearranged terminal is not the one they were looking at.

    PATH first and the bundle second, the reverse of `src-tauri/src/agterm.rs`:
    that lookup starts at the bundle because a Tauri app on macOS inherits no
    shell profile, while this runs from a terminal that does. The timeout is
    there for the same reason the dashboard's is — agtermctl talks to a control
    socket and a wedged agterm would otherwise hold this forever.

    THE ANSWER IS READ BEFORE THE EXIT CODE, which is the opposite of the usual
    order and is what makes a refusal legible. Every agterm-side refusal is an
    object on stdout — `{"ok": false, "error": "…"}` — with a non-zero exit and
    nothing on stderr at all. That is agterm's stated contract ("the process exit
    code is non-zero when `ok` is false"), and the review probed it with a stale
    `--window` id: exit 1, that object on stdout, stderr empty. So branching on
    the exit code first reports every refusal as a sentence that ends in an empty
    stderr, with the diagnosis discarded and the `ok` branch unreachable. The
    exit code is worth reading only where there is no JSON to read instead.
    """
    candidates = ["agtermctl", "/Applications/agterm.app/Contents/MacOS/agtermctl", "/opt/homebrew/bin/agtermctl", "/usr/local/bin/agtermctl"]
    for binary in candidates:
        try:
            r = subprocess.run([binary, *args, "--json"], capture_output=True, timeout=10)
        except FileNotFoundError:
            continue
        except subprocess.TimeoutExpired as e:
            raise dash.CaptureError(f"agtermctl {' '.join(args)} did not answer within 10s. Is agterm wedged?") from e
        out, err = r.stdout.decode("utf-8", "replace").strip(), r.stderr.decode("utf-8", "replace").strip()
        try:
            answer = json.loads(out)
        except json.JSONDecodeError as e:
            raise dash.CaptureError(f"agtermctl {' '.join(args)} exited {r.returncode} without a JSON answer: {err or out or 'it wrote nothing at all'}. Is agterm running?") from e
        if not answer.get("ok"):
            raise dash.CaptureError(f"agterm refused `{' '.join(args)}`: {answer.get('error') or answer}")
        if r.returncode != 0:
            raise dash.CaptureError(f"agtermctl {' '.join(args)} reported success and then exited {r.returncode}, which contradicts its own contract; this will not read past it. stderr: {err or '(empty)'}")
        return answer.get("result", {})
    raise dash.CaptureError("agtermctl is not on PATH and is not in agterm's bundle. Install it from agterm's Help > Install Command Line Tool.")


def pick_window(want: str | None) -> dict:
    """The open agterm window to photograph, or a refusal naming the alternatives.

    Ambiguity is refused rather than resolved by taking the frontmost, for the
    reason `dash.find_window` refuses it: a capture script that quietly picks a
    window photographs the wrong sidebar and nobody notices for a release.
    """
    windows = [w for w in agtermctl("window", "list").get("windows", []) if w.get("open") is not False]
    if want:
        windows = [w for w in windows if w["id"].lower().startswith(want.lower()) or w.get("name", "").lower() == want.lower()]
    if not windows:
        raise dash.CaptureError(f"No open agterm window matches {want!r}." if want else "agterm has no open window.")
    if len(windows) > 1:
        seen = ", ".join(f"{w['id']} ({w.get('name', '')!r})" for w in windows)
        raise dash.CaptureError(f"{len(windows)} agterm windows are open, so the capture will not guess which sidebar is the figure: {seen}. Name one with --window.")
    window = windows[0]
    if window.get("minimized"):
        raise dash.CaptureError(f"agterm window {window['id']} is minimized, so there is nothing on screen to photograph.")
    return window


def rendered(tree: dict) -> tuple[list[dict], list[tuple[str, dict]]]:
    """The workspace headers and session rows the sidebar is actually drawing.

    agterm's own rule, from its reference: a workspace row renders iff
    `sidebarVisible && sidebarMode == "tree" && (!workspaceFilter || focused)`,
    and a collapsed workspace draws its header without its sessions. Applying it
    is what keeps the checks below about what is in the frame rather than about
    what agterm happens to hold — counting a collapsed workspace's glyphs toward
    the spread would be a claim about a picture nobody can see.
    """
    filtered = bool(tree.get("workspaceFilter"))
    headers, rows = [], []
    for workspace in tree.get("workspaces", []):
        if filtered and not workspace.get("focused"):
            continue
        headers.append(workspace)
        if workspace.get("collapsed"):
            continue
        rows.extend((workspace.get("name", ""), session) for session in workspace.get("sessions", []))
    return headers, rows


def read_title(label: str) -> tuple[str | None, str]:
    """Split a sidebar label into the status its glyph names and the rest of it.

    The rule is `parse_title` in `src-tauri/src/terminal_title.rs`. An
    unrecognized leading token yields no status rather than a guess: a title this
    dashboard did not write says nothing about a row.
    """
    head, _, rest = label.strip().partition(" ")
    status = GLYPHS.get(head)
    return (status, rest.strip()) if status else (None, label.strip())


def names(rest: str, label: str) -> bool:
    """Whether a title's text is the one written for a row called `label`.

    Kept to the letter of `TitleReading::names`. It matches on a token boundary
    instead of stripping the known ` [N%]` and ` ⚠` suffixes, which is what stops
    it rotting when `build_title` grows a third one — and it is why a public
    `bga-assistant` does not vouch for a `bga-assistant-private` row.
    """
    return rest == label or rest.startswith(label + " ")


def vet(headers: list[dict], rows: list[tuple[str, dict]], public: list[str], min_glyphs: int, min_percent: int) -> list[dict]:
    """Refuse anything that would make this the wrong picture, before touching the machine.

    Three separate refusals, in the order they matter. A private name in frame is
    unrecoverable once published; a missing glyph spread or a missing percentage
    is a frame that illustrates nothing, and both of those look fine.

    The name check applies `dash.assert_publishable`'s rule through `names` rather
    than calling it. That helper compares a project name for equality, and a
    sidebar label carries whatever suffixes `build_title` appended, so the
    comparison here has to be the token-boundary one; a workspace header, which
    carries none, is compared for equality exactly as the helper would. The list
    itself is still the lib's — `--publishable` defaults to `dash.PUBLISHABLE_PROJECTS`.
    """
    read = []
    for workspace, session in rows:
        # The `name` field is agterm's own derived sidebar label — the custom name
        # when one is set, the OSC title otherwise. The dashboard never sets a
        # custom name on a session it starts, because that would permanently
        # outrank the title it writes and leave the row with no glyph at all.
        label = session.get("name") or session.get("title") or ""
        status, rest = read_title(label)
        percent = CONTEXT_SUFFIX.search(rest)
        read.append({"workspace": workspace, "label": label, "status": status, "rest": rest, "percent": int(percent.group(1)) if percent else None})

    # Workspace headers are names in the frame exactly as session rows are, so
    # they are vetted the same way rather than waved through for being chrome.
    # Reported as the text the rule compared, i.e. the label without its glyph,
    # rather than as the whole row: it is the half of the row an entry has to
    # vouch for, and the glyph is noise in a message about names.
    private = [w.get("name", "") for w in headers if w.get("name", "") not in public]
    private += [r["rest"] for r in read if not any(names(r["rest"], p) for p in public)]
    if private:
        raise dash.CaptureError(
            f"{len(private)} name(s) in the sidebar are not on --publishable: {', '.join(repr(p) for p in private)} (an entry is that name without any [N%] or ⚠ suffix). "
            "Every name in this frame is published verbatim, workspace headers included, so list one only if its repository is public — otherwise close or collapse it and run again."
        )

    spread = sorted({r["status"] for r in read if r["status"]})
    if len(spread) < min_glyphs:
        listed = ", ".join(f"{r['label']!r}" for r in read) or "(no session rows at all)"
        raise dash.CaptureError(
            f"The sidebar shows {len(spread)} distinct status glyph(s) ({', '.join(spread) or 'none'}), and this figure needs {min_glyphs}. Rows: {listed}. "
            "Stage sessions in different states — one mid-turn, one blocked on its user, one finished — or lower the bar with --min-glyphs."
        )

    carrying = [r["label"] for r in read if r["percent"] is not None]
    if len(carrying) < min_percent:
        raise dash.CaptureError(
            f"{len(carrying)} row(s) carry a [N%] context suffix ({', '.join(repr(c) for c in carrying) or 'none'}), and this figure needs {min_percent} — the count `screenshots.json` and the alt text in `docs/pages/features.md` both describe. "
            "A session has to be past `terminal_title_context_percent` of its model's window for the dashboard to write one, and the count comes from the transcript watcher, so it cannot be staged through the API. "
            "Wait for another session to get that far, or lower the bar with --min-percent and correct both descriptions to match."
        )
    return read


def sidebar_width_pt(window_id: str) -> float:
    """How wide agterm is drawing its sidebar, in logical points, from agterm.

    The one number that decides the frame's width, and it is read rather than
    assumed. The file is private state with no compatibility promise, so an
    unrecognized `version` refuses instead of trusting a field out of a layout
    this has never seen — the same stand-down the dashboard's own reader makes,
    and for the same reason: a confident wrong number here silently reframes a
    committed picture.
    """
    path = AGTERM_STATE / f"{window_id}.json"
    if not path.exists():
        raise dash.CaptureError(f"agterm has no saved state for window {window_id} (looked in ~/Library/Application Support/agterm/windows/). Pass --sidebar-width with the width in logical points.")
    state = json.loads(path.read_text(encoding="utf-8"))
    if state.get("version") != SNAPSHOT_VERSION:
        raise dash.CaptureError(f"agterm's window state is version {state.get('version')!r} and this reads version {SNAPSHOT_VERSION}; the layout may have moved. Pass --sidebar-width with the width in logical points.")
    width = state.get("sidebarWidth")
    if not isinstance(width, (int, float)):
        raise dash.CaptureError(f"agterm's window state carries no sidebarWidth ({path.name}). Pass --sidebar-width with the width in logical points.")
    return float(width)


# ------------------------------------------------------------------ the pixels

def background(im: Image.Image, box: tuple[int, int, int, int]) -> tuple[int, int, int, int]:
    """The sidebar's flat background colour, as the commonest colour in the region.

    Taken over the whole region rather than sampled at a chosen point: a point
    can land on the selected row's highlight, on a glyph or on a badge, while the
    background is by far the largest area of a sidebar. The `getcolors` call is
    given room for every pixel to be distinct, so it cannot return `None` and
    fall through to a guess.
    """
    region = im.crop(box)
    counted = region.getcolors(region.width * region.height + 1)
    return max(counted)[1]


def row_has_content(im: Image.Image, y: int, x0: int, x1: int, bg: tuple, tol: int = 12) -> bool:
    """Whether one row of the sidebar holds anything but its own background."""
    row = im.crop((x0, y, x1, y + 1))
    return any(a > 250 and max(abs(c - b) for c, b in zip((r, g, bl), bg[:3])) > tol for _, (r, g, bl, a) in row.getcolors(x1 - x0 + 1))


def last_content_row(im: Image.Image, right: int, inset: int, border: int) -> int:
    """The bottom of the sidebar's content, scanning up from the window's bottom edge.

    The scan is inset on both sides and lifted off the bottom for the same reason
    the Windows script starts its strip measurement at row 8: the window's own
    border differs from the background at every column, so a scan that includes it
    calls every row content and measures nothing.

    Caveat it fails safe on: agterm can draw a bar at the foot of the sidebar, and
    where it does this reports that bar rather than the last session row. That
    under-reports dead space and never over-reports it, so the check below can
    miss a badly-sized window but cannot refuse a well-sized one.
    """
    bg = background(im, (inset, im.height // 2, right - inset, im.height - border))
    for y in range(im.height - 1 - border, -1, -1):
        if row_has_content(im, y, inset, right - inset, bg):
            return y
    raise dash.CaptureError("The sidebar measured empty — every row matches its own background. Nothing was written.")


# ------------------------------------------------------------------------ main

def main() -> int:
    # allow_abbrev=False for the reason the Windows scripts all carry
    # [CmdletBinding()]: a flag that is accepted, ignored and silently dropped is
    # how a run meant as a probe overwrote a committed frame.
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("--publishable", nargs="+", default=list(dash.PUBLISHABLE_PROJECTS), metavar="NAME", help=f"every name allowed to appear in the sidebar — session labels and workspace headers. Anything else refuses the capture. Defaults to the lib's checked-public list ({', '.join(dash.PUBLISHABLE_PROJECTS)}), which names no workspaces.")
    parser.add_argument("--window", metavar="ID", help="which agterm window, by id prefix or name; only needed when more than one is open")
    parser.add_argument("--sidebar-width", type=float, metavar="PT", help="the sidebar's width in logical points, for when agterm's own state cannot be read")
    parser.add_argument("--min-glyphs", type=int, default=3, metavar="N", help="distinct status glyphs the sidebar must show (default 3; the committed frame carries four)")
    parser.add_argument("--min-percent", type=int, default=2, metavar="N", help="rows that must carry a [N%%] context suffix (default 2, the count the manifest and the alt text describe)")
    parser.add_argument("--allow-cut", action="store_true", help="crop a full-size window down to the sidebar, accepting the square right corners")
    parser.add_argument("--probe", action="store_true", help="write to tmp/ instead of the committed frame, for checking the framing first")
    args = parser.parse_args()
    try:
        return capture(args)
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        return 1


def capture(args: argparse.Namespace) -> int:
    window = pick_window(args.window)
    tree = agtermctl("tree", "--window", window["id"]).get("tree", {})
    if not tree.get("sidebarVisible"):
        raise dash.CaptureError(f"agterm's sidebar is hidden in window {window['id']}. This figure is the sidebar; show it from the toggle at the right of agterm's title bar and run again.")
    if tree.get("sidebarMode") != "tree":
        raise dash.CaptureError(f"agterm's sidebar is in {tree.get('sidebarMode')!r} mode, which draws a flat flagged list with no workspace rows. Switch it back to the tree.")
    # A third render condition, separate from the two above because agterm's
    # reference describes it separately: `surface zoom` fills the window with one
    # terminal and hides the sidebar, while `sidebarVisible` is the read side of
    # the `sidebar` command alone. Whether zooming also drops `sidebarVisible`
    # was NOT tested — testing it means zooming a surface, which is driving the
    # operator's terminal — so this stands on its own rather than on that guess.
    # Without it a zoomed window passes both gates and the crop takes the
    # leftmost strip of terminal text, saved to the committed path as "the
    # sidebar": exactly the plausible wrong picture there is no `--force` for.
    if tree.get("zoomedSurface"):
        raise dash.CaptureError(f"A terminal surface is zoomed in window {window['id']} ({tree['zoomedSurface']!r}), which fills the window and hides the sidebar, so there is no sidebar on screen to photograph. Exit the zoom and run again.")

    headers, rows = rendered(tree)
    read = vet(headers, rows, args.publishable, args.min_glyphs, args.min_percent)
    for row in read:
        glyph = next((g for g, s in GLYPHS.items() if s == row["status"]), " ")
        print(f"  {row['workspace']:<10} {glyph} {row['label']}" + ("" if row["status"] else "   <- no title from this dashboard; the row shows no glyph"))

    # Read before anything is hidden or photographed, like the vetting above: a
    # refusal that costs nothing has not touched the user's machine at all.
    width_pt = args.sidebar_width if args.sidebar_width else sidebar_width_pt(window["id"])
    if not PLAUSIBLE_PT[0] <= width_pt <= PLAUSIBLE_PT[1]:
        raise dash.CaptureError(f"The sidebar measured {width_pt:.1f}pt, outside the plausible {PLAUSIBLE_PT[0]}–{PLAUSIBLE_PT[1]}pt — that is a broken measurement rather than a dragged sidebar. Pass --sidebar-width to override it.")
    if abs(width_pt - COMMITTED_PT[0]) > 12:
        print(f"NOTE: the sidebar has been dragged to {width_pt:.1f}pt, so this frame will not be the width of the one it replaces ({COMMITTED_PT[0]}pt).")

    # The selected session's label is what agterm publishes as the window's title,
    # so matching on it ties the pixels to the tree that was just vetted — and it
    # keeps agterm's other on-screen surfaces (the quick terminal, a settings
    # window) from tying with the one we want. Read from the whole tree rather
    # than from the rendered rows, since the selected session can sit inside a
    # collapsed workspace; with no answer at all the match falls back to the owner
    # alone, which `find_window` refuses if that is ambiguous.
    sessions = [s for w in tree.get("workspaces", []) for s in w.get("sessions", [])]
    selected = next((s.get("name") for s in sessions if s.get("active")), None)
    raw = dash.probe_path(f"{FIGURE}-raw")
    with dash.widget_hidden():
        win = dash.find_window(AGTERM, title=selected)
        dash.capture_window(win, raw)

    with Image.open(raw) as opened:
        im = opened.convert("RGBA")
        # Re-derived, not re-judged: `capture_window` returned, so it has already
        # refused anything that is not this window at exactly 1x or 2x — and its
        # refusal names Screen Recording permission, which is the cause a second
        # copy of the rule here would have had to name again and drift from.
        scale = round(im.width / win["w"])

        print(f"agterm window {window['id']} {int(win['w'])}x{int(win['h'])}pt, captured {im.width}x{im.height} at {scale}x; sidebar {width_pt:.1f}pt (the committed frame is {COMMITTED_PT[0]}x{COMMITTED_PT[1]}pt)")

        right = min(im.width, round(width_pt * scale))
        inset, border = INSET_PT * scale, BORDER_PT * scale
        bottom = min(im.height, last_content_row(im, right, inset, border) + 1 + BOTTOM_PAD_PT * scale)
        cut_w, cut_h = im.width - right, im.height - bottom
        snug = cut_w <= EDGE_SLACK_PT * scale and cut_h <= TAIL_SLACK_PT * scale
        if snug:
            # The window is its sidebar: what is left over is agterm's own border
            # and the margin under the last row, both of which belong in frame.
            right, bottom = im.width, im.height
        elif not args.allow_cut:
            raise dash.CaptureError(
                f"agterm's window is {cut_w // scale}pt wider and {cut_h // scale}pt taller than its sidebar, so cropping it would end the frame on a hard cut with two square corners where the frame it replaces has four round ones. "
                f"Size agterm's window to about {round(width_pt) + 2}x{round(bottom / scale)}pt by hand and run again, or pass --allow-cut to take the cropped frame anyway."
            )

        out = dash.probe_path(FIGURE) if args.probe else dash.shot_path(FIGURE)
        # Carry the ICC profile across the crop by hand. Pillow reads it into
        # `info` and writes it back only when it is handed over explicitly, so a
        # plain `save` drops the sRGB tag `capture_window` has just converted the
        # file to -- and this is the only figure whose script writes the PNG a
        # second time, so it would be the only untagged macOS frame. An untagged
        # PNG is not a cosmetic problem here: `check-figures.py` reads the traffic
        # lights' saturation off the raw numbers, and the numbers only mean what
        # that gate assumes if they are sRGB.
        im.crop((0, 0, right, bottom)).save(out, icc_profile=im.info.get("icc_profile"))

    kind = "the whole window; it is already its sidebar" if snug else f"cropped {cut_w}x{cut_h} device px off the right and bottom"
    print(f"{out.relative_to(dash.REPO)}  {right}x{bottom}  ({kind})")
    # The uncut capture stays in tmp/, which is scratch and never committed: it is
    # what the cut is judged against when the framing looks wrong.
    print(f"{raw.relative_to(dash.REPO)}  the capture before any crop")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
