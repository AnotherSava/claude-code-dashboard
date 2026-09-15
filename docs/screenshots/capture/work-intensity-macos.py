#!/usr/bin/env python3
"""macOS variant of the `work-intensity` figure.

Opens the Work intensity window on a chosen week, sizes it wide enough that the
header stays on one line, and photographs it with its title bar.

THE SUBJECT IS A WEEK, AND IT IS PINNED BY DATE: Aug 31 - Sep 6, 2026. That is
the week work-intensity-windows shows, and the week this script's 2026-09-11
re-shoot put in docs/screenshots/work-intensity-macos.png. A paired figure is
read as two pictures of one thing, so two different weeks make it a puzzle
instead of a comparison.

That is why the week is a DATE and not the offset the window's API actually
takes. An offset counts weeks back from whenever the script runs, so it names a
different week every week while looking like a constant; --week-start is resolved
against today at run time, the offset derived from it, and the resolution printed
so the operator can see which week it picked. Pass --offset only to look around,
and only together with --probe, which writes to tmp/; --offset on its own is
refused rather than allowed to overwrite the pinned frame with another week. The
chart is the one figure here whose content is a moving window over live data, and
the prose beside it was written against this week's bars and the numbers in its
right margin, which only a week that was actually busy has at all.

WIDTH IS 1520 LOGICAL PX, AND IT IS NO LONGER ABOUT THE HEADER. The Navigation
and Legend hints that used to sit there are gone, and what is left -- the date
range, the three totals under one "This week:" and Days/Weeks -- fits on one
line down to about 900px, so the old collide-below-1280 / wrap-at-1320
thresholds no longer describe anything. The width now buys horizontal resolution:
144 ten-minute slots have to be told apart across the plot, and a day's shape is
what the figure is arguing. 1520 is also what the Windows variant uses -- the
same pairing argument as the week -- and --height follows its 700 for the same
reason: two frames of one chart are easiest to read against each other when the
chart has the same proportions in both.

A 1520pt WINDOW DOES NOT FIT THIS DISPLAY, which is 1470pt wide, so it cannot sit
fully on screen at this width. That turned out to be harmless: `screencapture -l`
reads the window's backing store rather than the screen, and the capture came
back the full 3040px, 1520 at 2x.

WHAT THE OPERATOR STAGES BY HAND, since no argument here can do it:

  * Deploy the current build. The frame documents what ships, and this script
    photographs whatever binary is running.
  * Leave the mouse pointer off the chart window. Hovering a bar raises a
    tooltip, which then sits in the committed frame. The Windows half parks the
    pointer clear of the window; this one only LOOKS -- moving the pointer is
    taking the machine over, which a capture script should not do to a user who
    asked for a screenshot -- and refuses when the pointer is inside the window's
    rect at the moment of the shutter. Pointer-outside is a sound test for "no
    tooltip": the tooltip is raised on the canvas's mousemove and cleared on its
    mouseleave. The one hole is a resize that moves the window out from under a
    pointer that never moves, where whether WebKit synthesizes the mouseleave is
    not established either way. And if the pointer cannot be read at all the run
    says so on stderr and goes ahead UNCHECKED, rather than passing silently.
There is deliberately no --force. Each refusal has a one-action remedy (move the
pointer; add --probe), so an override could only ever defeat a guard that was
right. The pointer guard also runs for a --probe: a probe that skips it is not a
rehearsal of the shot.

Nothing in this frame carries a session's name or prompt text, so the rule that
every session visible in a frame be a public repo has nothing to bite on here --
unlike the widget frames, this one calls no `dash.assert_publishable`.

NOTHING HERE NEEDS A HAND FINISH. The lib's `capture_window` shoots with the
`screencapture -o` that leaves the compositor's drop shadow out of the image
altogether, then runs `trim_halo.py` and converts the PNG to sRGB with `sips`.
So the one-pixel transparent corner ring the manifest used to record against this
frame cannot form -- it was the residue of trimming a shadow that is no longer in
the picture -- and the title bar's traffic lights stay in the colour space that
`check-figures.py` reads them in, the display's wide-gamut profile putting the
green one under that gate's saturation floor. The manifest entry for this figure
names this script and carries neither hand step.

WHAT OUTLIVES THE RUN, and what is put back. The chart window is left on screen,
exactly as the Windows half leaves it: the window API can hide the widget and
nothing else, and closing the chart is one click. Its size is put back in the
`finally`, and so is the week: `IntensityApp` holds the week offset and the view
as component state, and the chart webview is hidden rather than destroyed on
close, so a week this script asked for would otherwise be the week the user's
chart opens on for the rest of the app's life. The Days/Weeks toggle is the one
thing NOT restored -- no API reads it back, so a run leaves the chart on Days
rather than on a guess.
"""
import argparse
import ctypes
import ctypes.util
import sys
import time
from datetime import date, datetime, timedelta
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
import dashboard as dash  # noqa: E402  (the lib is found only once sys.path is set)

FIGURE = "work-intensity-macos"
TITLE = "Work intensity"        # the intensity window's title, from tauri.conf.json
OPEN_SETTLE_S = 0.8             # the window comes up, then fetches and draws the week
RESIZE_SETTLE_S = 2.5           # bars animate in, and the header only re-decides its layout once the window has settled
FOCUS_SETTLE_S = 0.6            # let the raise finish before the shutter; see the block that spends it


def resolve_offset(week_start: str) -> int:
    """Weeks back from this week's Monday, resolved from a date at run time.

    Weeks run Monday to Sunday in LOCAL time, matching `local_week_start_ms` in
    the Rust, which takes today's local date and subtracts its weekday. Both ends
    here are `date`s and therefore carry no clock, so a DST change cannot make the
    span a non-integer number of weeks -- which is why this divides where the
    Windows half rounds, that one subtracting DateTimes.
    """
    try:
        want = datetime.strptime(week_start, "%Y-%m-%d").date()
    except ValueError as e:
        raise dash.CaptureError(f"--week-start must be a date as YYYY-MM-DD; got {week_start!r}.") from e
    if want.weekday() != 0:
        raise dash.CaptureError(f"--week-start must be a Monday; {week_start} is a {want.strftime('%A')}.")

    today = date.today()
    this_monday = today - timedelta(days=today.weekday())
    offset = (want - this_monday).days // 7
    print(f"Week {week_start} resolves to offset {offset} from the week of {this_monday:%Y-%m-%d}.")
    if offset > 0:
        raise dash.CaptureError(f"--week-start {week_start} is in the future.")
    return offset


class _CGPoint(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


def pointer_location() -> tuple[float, float] | None:
    """Where the pointer is, in the top-left-origin global points `window_list.swift` reports, or None if it cannot be read.

    Calling `CGEventCreate(NULL)` snapshots the current event state, the pointer
    included. It reads and never writes, so it needs no Accessibility grant and
    takes nothing over: this script looks at the pointer and leaves it exactly
    where the operator put it.

    ctypes rather than pyobjc because ctypes is in the standard library, and a
    guard is not worth a dependency. CGPoint is two doubles and comes back in the
    floating-point registers, which libffi handles; `restype` has to be declared
    or it is truncated to an int.
    """
    try:
        path = ctypes.util.find_library("ApplicationServices")
        if not path:
            return None
        cg = ctypes.cdll.LoadLibrary(path)
        cg.CGEventCreate.restype, cg.CGEventCreate.argtypes = ctypes.c_void_p, [ctypes.c_void_p]
        cg.CGEventGetLocation.restype, cg.CGEventGetLocation.argtypes = _CGPoint, [ctypes.c_void_p]
        cg.CFRelease.argtypes = [ctypes.c_void_p]
        event = cg.CGEventCreate(None)
        if not event:
            return None
        try:
            at = cg.CGEventGetLocation(event)
        finally:
            cg.CFRelease(event)
        return (at.x, at.y)
    except (OSError, AttributeError):
        return None


def assert_pointer_clear(win: dict) -> None:
    """Refuse when the pointer sits over the window about to be photographed."""
    at = pointer_location()
    if at is None:
        print("WARNING: the pointer position could not be read, so this run is UNCHECKED for a hover tooltip — look at the frame before committing it.", file=sys.stderr)
        return
    x, y = at
    if win["x"] <= x <= win["x"] + win["w"] and win["y"] <= y <= win["y"] + win["h"]:
        raise dash.CaptureError(f"The pointer is at ({x:.0f}, {y:.0f}), inside the chart window at ({int(win['x'])}, {int(win['y'])}) {int(win['w'])}x{int(win['h'])}. Hovering a bar raises a tooltip that would be in the frame. Move the pointer off the window and run this again.")


def restore_size(before: dict, asked_height: int) -> None:
    """Put the window back to the size it had, compensating for the title bar.

    Tauri sizes the CONTENT (`set_size` is tao's `set_inner_size`) while
    CoreGraphics measures the whole FRAME, so the two differ by the title bar.
    Handing the measured frame height straight back would leave the window a title
    bar taller after every run; the gap between the height that was asked for and
    the height that came back measures it instead. Width needs no correction --
    a titled macOS window has no side chrome.

    Failures here are reported and swallowed, and the catch is deliberately broad
    rather than `CaptureError` alone: this runs in a `finally`, and anything that
    raised -- a window that has vanished, a helper whose output will not parse --
    would replace whatever really went wrong with itself.
    """
    try:
        now = dash.find_window(dash.APP, title=TITLE)
        chrome = now["h"] - asked_height
        if not 0 <= chrome <= 120:
            print(f"WARNING: the window measures {int(now['h'])}pt tall after a {asked_height}pt content resize, which is not a title bar's worth of difference. Leaving it at the capture size.", file=sys.stderr)
            return
        dash.window(action="resize", label="intensity", width=before["w"], height=before["h"] - chrome)
    except Exception as e:
        print(f"WARNING: the chart window was left at the capture size ({e})", file=sys.stderr)


def reset_week() -> None:
    """Put the chart back on the current week, which no other path would do.

    The week offset and the view are component state in `IntensityApp`, and the
    chart webview is hidden rather than destroyed on close, so the week asked for
    here is the week the user's chart opens on until the app restarts. The view
    cannot be read back through any API, so this leaves it on Days instead of
    guessing at what the user had.

    Failures are reported and swallowed for the reason `restore_size` gives.
    """
    try:
        dash.window(action="intensity", offset=0, view="day")
    except Exception as e:
        print(f"WARNING: the chart window was left on the week this run photographed ({e})", file=sys.stderr)


def main() -> int:
    ap = argparse.ArgumentParser(description="Capture the Work intensity figure on macOS.", allow_abbrev=False)
    ap.add_argument("--week-start", default="2026-08-31", help="Monday of the week to photograph, YYYY-MM-DD (default: the week the Windows variant shows)")
    ap.add_argument("--offset", type=int, help="weeks back from this one, overriding --week-start; for looking around, so it requires --probe. A positive value is refused by the window API as future_week")
    ap.add_argument("--width", type=int, default=1520, help="content width in logical px (default: 1520, measured here to keep the header on one line)")
    ap.add_argument("--height", type=int, default=700, help="content height in logical px (default: 700, the Windows variant's)")
    ap.add_argument("--probe", action="store_true", help="write to tmp/ instead of the committed frame, for iterating on a week or a width")
    args = ap.parse_args()

    if args.offset is not None and not args.probe:
        raise dash.CaptureError(f"--offset {args.offset} is for looking around, and without --probe this run would overwrite the committed frame with whatever week that is — the pair is pinned to one week. Add --probe to write to tmp/, or move the pin with --week-start.")

    if args.offset is not None:
        offset, week = args.offset, f"offset {args.offset}"
        print(f"Using the explicit offset {offset}; --week-start {args.week_start} is ignored.")
    else:
        offset, week = resolve_offset(args.week_start), f"week {args.week_start}"

    out = dash.probe_path("work-intensity-probe") if args.probe else dash.shot_path(FIGURE)

    with dash.widget_hidden():
        # `before` is bound ahead of the try so the `finally` can tell "never
        # measured" from "measured": the size is only restorable once it has been
        # read back, and a failure before that must still reset the week.
        before = None
        # The try opens BEFORE the week is asked for, not after. Everything
        # between the two is a step that can fail -- `find_window` refuses on an
        # ambiguous or missing window, `resize` on a window the API cannot find --
        # and the chart is already showing the photographed week by then, so a
        # `try` opened any later would leave the user's chart parked on it.
        dash.window(action="intensity", offset=offset, view="day")
        try:
            time.sleep(OPEN_SETTLE_S)
            before = dash.find_window(dash.APP, title=TITLE)  # measured now, so the restore aims at the size the user had
            dash.window(action="resize", label="intensity", width=args.width, height=args.height)
            time.sleep(RESIZE_SETTLE_S)
            # Using `dash.shot` would find, capture and report in one call; the
            # pointer guard has to read the rect the shutter is about to
            # photograph, so the halves are called separately here.
            #
            # Raise the window one last time. It was focused when `intensity`
            # opened it, but seconds of resizing and settling pass before the
            # shutter, and the terminal running this script takes the focus back
            # in between.
            #
            # It is NOT here to stop washed-out traffic lights, which is the
            # reason it was first written for and which was wrong. The two
            # frames that comparison rested on differ by colour profile and not
            # by focus: measured on this window's own captures, the green light
            # reads saturation 0.49 in the display's wide-gamut profile and 0.73
            # after conversion to sRGB, and `check-figures.py` identifies a macOS
            # frame by those three dots over a 0.55 floor. The lib's
            # `capture_window` converts to sRGB as its last step, and that is
            # what keeps the frame readable to that gate.
            #
            # What is left as a reason to raise it: macOS draws an inactive
            # window's traffic lights grey, and grey is under that same floor.
            # That has NOT been measured here, so the call stays as a cheap
            # precaution -- one API call and FOCUS_SETTLE_S -- and not as a
            # demonstrated need.
            dash.window(action="show", label="intensity")
            time.sleep(FOCUS_SETTLE_S)
            win = dash.find_window(dash.APP, title=TITLE)
            assert_pointer_clear(win)
            dash.capture_window(win, out)
            # This window is decorated and STILL has no frame of its own, which
            # is the case that makes "decorated windows need nothing" the wrong
            # rule to code against. It carries `"theme": "Dark"`, and a
            # dark-themed NSWindow gets no light stroke: measured, its top row is
            # the title bar's own highlight while its other three sides are the
            # chart background. `add_hairline` re-checks all four edges and
            # declines if it finds one, so asking is safe and not asking is not.
            dash.add_hairline(out)
            print(f"{out.relative_to(dash.REPO)} <- {TITLE}, {week} ({int(win['w'])}x{int(win['h'])} logical)")
        finally:
            if before is not None:
                restore_size(before, args.height)
            reset_week()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        raise SystemExit(1)
