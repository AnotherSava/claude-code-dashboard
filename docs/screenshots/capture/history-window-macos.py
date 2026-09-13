#!/usr/bin/env python3
"""macOS variant of the `history-window` figure.

Opens the History window on one session and photographs it with its native title
bar. The window's title is the session's display name, not the word "History",
so the shot is matched on the name this script just asked for.

THE SUBJECT IS FIXED: the bga-assistant session (AnotherSava/bga-assistant,
public), which is what history-window-windows shows. The two variants are read
as one figure, so they photograph the same conversation; a re-shoot that picks
whatever session is handy turns the pair into two unrelated pictures. The
previous macOS frame is that session too, so this script re-shoots what it
replaces rather than moving the subject.

THE ROW ID DIFFERS PER MACHINE, THE TITLE DOES NOT. A row id is derived from the
session's cwd by `adapters::claude::derive_chat_id`, and with `projects_root`
unset — it is unset on both machines — that is the directory basename. This Mac
checks the repo out into a directory called `bga-assistant`, so the id here is
that word; the Windows box checks it out into one called `assistant`, so the id
there is `assistant` and the title bar reads `bga-assistant` only because a
custom name supplies it. The title resolves through `commands::history_title`
— the rename store when it holds an entry for the row, the row id otherwise — so
both frames carry the same words while --session differs between the two
scripts. That is also why the title is read off the roster below instead of
being assumed from the id.

CHOOSING A SESSION IS HALF CHECKABLE. The project name goes in the title bar, so
that half is mechanical: `--publishable` names every project allowed there and the
capture refuses anything else, defaulting to `dash.PUBLISHABLE_PROJECTS`. The rest is
a content question no flag can answer, so this is what the choice had to
satisfy, kept for the day the subject must move: the dialog must be publishable
to its last visible line, and the conversation must carry a reply long enough to
fold. The list scrolls to the BOTTOM on open, so only the tail is in frame —
which is what makes a session used for staging other screenshots the worst
choice, its tail being the staging prompts. Read the tail before capturing; it
sits in the `prompt_history.json` file in the app data dir, which is far quicker
to search than opening windows.

--height IS THE SCROLL POSITION, which is the other half of the subject. The
list is bottom-anchored and cannot be scrolled without driving the machine, so
the window's height is what decides where the visible history starts: shrink it
and the top of the frame moves down the conversation. 895 WAS NEVER MEASURED ON
macOS — it is arithmetic off the frame it replaces, which is 1846px tall at 2x,
so 923 logical, less the 28pt title bar that `screencapture` includes and the
resize does not (the API sets the content size). It reproduces the shape of the
picture already on the docs page and nothing more, which is why a run without an
explicit --height is refused the committed frame and may only write a probe: an
unmeasured number must not decide what the docs page shows. The Windows number
cannot be transplanted either: 310 was measured against that machine's text
metrics, one line higher putting an absolute path in frame and one line lower
dropping the `<...>` fold the figure exists to show. Measure the macOS number
the same way on the first real run — probe, look at the top edge and at the
tail, move the number until the frame starts on a clean line, carries no path
and keeps a fold — then pin it here with what it was measured against and delete
the guard in the same commit, the way the Windows header records its 310.

A CAPTURE CAN COME BACK WITH NO FOLD AT ALL, and no height fixes that. Measured
2026-09-11 against the subject's stored dialog (470 entries held, nothing
pruned): the most recent entry that folds is index 456, a 70-line 5721-char
assistant reply, 13 entries from the end. The 12 entries after it run 10-16
lines and 1-975 chars, under every threshold in `computeFold`
(src/HistoryApp.svelte), so none of them folds. The list is bottom-anchored and
/api/window deliberately exposes no scroll, so height is the only lever, and
pulling entry 456 into frame needs roughly twice the 923 logical pixels this
display allows. The remedy is a long reply at the tail — ask that session
something whose answer folds, then capture — rather than a taller window.

WHAT THE OPERATOR STAGES BY HAND: deploy the current build, so the frame shows
the working tree rather than the installed release; read the session's tail
first, per the requirements above; and run this from a terminal that holds
Screen Recording permission, without which `screencapture` writes a picture of
the wallpaper instead of the window (the lib checks the size it got back and
refuses, so this fails loudly rather than committing a desktop).

Probe with --probe until the frame is right, then pass the height you settled on
and take the shot once. A probe writes to `tmp/` and never to the committed
frame, so iterating on the height cannot leave a half-tuned picture in the repo. Unlike the Windows -Method
switch, it selects no second capture path — `screencapture -l<id>` photographs a
window wherever it sits and never takes the foreground, so every run here costs
the same nothing — the switch is the destination alone.

One defect recorded against the previous frame needs no handling here: the
fully transparent one-pixel ring inside all four rounded corners was the residue
of trimming the compositor's drop shadow, and `dash.capture_window` drops the
shadow at source with `-o`, so there is nothing for the trim to eat into. That
same call converts the result to sRGB after the trim, which matters more for
this figure than for the others: `screencapture` tags its output with the
display's wide-gamut profile, and `check-figures.py` recognizes a macOS frame by
the saturation of the three traffic lights — the very title bar this figure is
taken for — so an unconverted capture is a decorated macOS window the checker
cannot confirm is one.
"""
import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))   # the lib sits beside this file, so the insert has to precede the import
import dashboard as dash  # noqa: E402

FIGURE = "history-window-macos"
SUBJECT = "bga-assistant"

# The height the probe falls back to, and the only number here that nothing
# checked: see the --height paragraph in the module docstring. It is arithmetic
# off the frame this figure replaces, never measured against macOS text metrics,
# which is why it cannot reach the committed frame on its own.
UNVERIFIED_HEIGHT = 895

# The window opens maximized and then pulls its dialog in; resizing before that
# finishes leaves the text reflowing under the capture.
OPEN_SETTLE_S = 2.0

# Longer than the lib's default 700ms, which is sized for the widget animating
# its rows in. This window re-wraps a screenful of text at the new width, and it
# does it after the resize returns.
SHOT_SETTLE_MS = 1500


def resolve_title(session: str, public: list[str]) -> str:
    """The title the capture matches on, or a refusal naming the rows that exist.

    Refusing is the point: an id the dashboard does not know opens a window with
    nothing in it, and an empty History window is a perfectly plausible-looking
    screenshot of the wrong thing.

    The row that is found is then checked against the public list, because this
    frame publishes more than any other in the set: the project name in the title
    bar and the tail of the conversation under it. Both strings that can reach
    that title bar are checked — the project name, and the rename that outranks
    it when the store holds one — so `--session what-is-next` is refused here
    rather than in a review of a picture nobody can diff.
    """
    roster = dash.agents()
    rows = roster.get("agents", [])
    for row in rows:
        if row.get("id") == session:
            title = row.get("display_name") or row["id"]
            dash.assert_publishable({title, row.get("project") or row["id"]}, allowed=public, where="the title bar, with the conversation under it")
            return title

    live = ", ".join(sorted(r.get("id", "") for r in rows)) or "(no rows at all)"
    # A live session the dashboard has never classified sits in `registry_only`
    # rather than in `agents`, and that is a different repair from a mistyped id:
    # the session is there, it just has not spoken to this dashboard since it
    # started, so the row arrives the moment it next acts.
    registry = {r.get("id", "") for r in roster.get("registry_only", []) if r.get("local")}
    hint = " It is live in Claude Code's own session list but has no row yet, so give it a turn — any prompt in that directory — and run this again." if session in registry else ""
    raise dash.CaptureError(f"No row with id {session!r}. Live rows: {live}.{hint}")


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description="Capture the History window for one session (macOS).")
    parser.add_argument("--session", default=SUBJECT, help=f"Row id as GET /api/agents reports it (default: {SUBJECT}, the fixed subject of this figure)")
    parser.add_argument("--publishable", nargs="+", metavar="NAME", default=list(dash.PUBLISHABLE_PROJECTS), help=f"Project names allowed in the title bar; replaces the built-in list rather than extending it (default: {', '.join(dash.PUBLISHABLE_PROJECTS)})")
    parser.add_argument("--width", type=int, default=900, help="Content width in logical pixels (default: 900, matching the Windows variant)")
    parser.add_argument("--height", type=int, default=None, help=f"Content height in logical pixels — this is the scroll position. Required for the committed frame; a probe left without it uses {UNVERIFIED_HEIGHT}, which was never measured on macOS.")
    parser.add_argument("--probe", action="store_true", help="Write to tmp/ instead of the committed frame, for tuning the height")
    args = parser.parse_args()

    # The guard reads whether --height was passed, not what it says, so a
    # measurement that comes out at 895 is still expressible. Pinning the number
    # is editing UNVERIFIED_HEIGHT into a measured default and deleting this
    # block.
    if args.height is None and not args.probe:
        raise dash.CaptureError(
            f"--height has never been measured on macOS, so a bare run will not overwrite {dash.shot_path(FIGURE).name}. The height is the scroll position: "
            f"{UNVERIFIED_HEIGHT} is arithmetic off the frame this replaces rather than a number checked against text, and on Windows one line out put an absolute "
            "path in frame. Probe first (--probe --height N, until the top edge starts on a clean line, the tail carries no path and a <...> fold is in frame), then "
            "pass that number as --height to take the shot."
        )
    height = args.height if args.height is not None else UNVERIFIED_HEIGHT

    title = resolve_title(args.session, [n.strip() for n in args.publishable if n.strip()])
    out = dash.probe_path(f"{FIGURE}-probe") if args.probe else dash.shot_path(FIGURE)

    dash.window(action="history", id=args.session)
    time.sleep(OPEN_SETTLE_S)
    try:
        dash.window(action="resize", label="history", width=args.width, height=height)
        # The widget is hidden only now, after the window is open: asking for a
        # history window reveals the widget on purpose — one is opened *from* a
        # row — so hiding it first would simply be undone.
        with dash.widget_hidden():
            dash.shot(dash.APP, out, title=title, settle_ms=SHOT_SETTLE_MS)
    finally:
        # Put the window back the way the user finds it. The `save_window_position`
        # setting is on by default and `lib.rs` saves this window's geometry when it
        # is closed — so left at the capture size, that size becomes what every later
        # click on a row opens, a sliver instead of the maximized window `open_history`
        # gives when nothing is saved. Maximizing keeps the capture's geometry off disk
        # entirely: the save records `maximized` and deliberately leaves the last
        # unmaximized bounds alone.
        dash.window(action="maximize", label="history")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        sys.exit(1)
