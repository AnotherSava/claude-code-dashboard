#!/usr/bin/env python3
"""macOS variant of the `hero` figure — the image on the README and the docs index.

Its Windows sibling is `hero-windows.ps1`, and the two are read as one figure:
the same widget, the same spread of states, photographed on two operating
systems. A hero that argues something different on each OS is two pictures
instead of one comparison, so the SUBJECT is held identical.

THE GUARD IS NOT, and the difference is deliberate rather than drift. The
Windows one checks two things, the spread and whether a peer is connected; this
one checks four. It refuses a project not known to be public, because this is the
figure that publishes a row's prompt text and not only its name. It decides the
synced question on rows rather than on the peer list, because a connected peer
contributing no rows puts nothing in the picture. And it reads the stored usage
sample from disk, because a frame whose bars say `--%` is the one failure the
roster cannot report. Those three are checks the Windows half could also carry
and does not; they are noted here rather than quietly diverging, and the two
scripts are worth reconciling the next time either is touched.

THERE IS NO CAPTURE METHOD TO CHOOSE HERE, which is the one place this file is
much shorter than its sibling. The Windows script has to say `-Method Alpha`
because nothing on Windows returns a window with its alpha, so the window is
photographed twice over two backdrops and the per-pixel alpha solved; a
client-area capture, which was tried first, drops the border and squares off the
corners. On macOS `screencapture -l<id> -o` writes the window at device
resolution with real per-pixel alpha and no drop shadow in one call, and the lib
finishes every capture with `trim_halo.py` and then a `sips` conversion to sRGB,
both unconditional. So the border curving round the corners, the transparency
outside them, and pixels in the space the other committed frames are stored in
all come out of `dash.shot` with nothing asked for. The conversion is there
because `screencapture` tags its output with the display's wide-gamut profile,
which desaturates every stored number and stopped `check-figures.py` recognising
macOS window chrome; that checker cannot rule on this frame either way, the
widget being undecorated ("no window chrome to read"), so what the hero gets out
of the conversion is only that its pixels are measured in the same space as every
other frame. Run it from a terminal holding Screen Recording permission: without
it `screencapture` quietly photographs the wallpaper, and the size check inside
the lib's `capture_window` is what catches that.

WHAT THIS SCRIPT CANNOT DO is put the widget into the state the frame has to
show: one row blocked on its user, one working, one finished. Those are real
sessions in real states, staged by hand (below). The script checks the spread is
there and refuses rather than capturing whatever happens to be on screen — a
hero that quietly shows three idle rows is worse than no hero, because nothing
about it looks wrong.

THE GUARD READS A DIFFERENT SNAPSHOT THAN THE FRAME PHOTOGRAPHS, and on this
platform the gap between them is routine rather than a corner case. The roster
at `/api/agents` is served from `commands::resolved_snapshot`; the widget
draws `display_snapshot`, which is that plus `apply_read_as_idle`. So a DONE row
the user has already looked at still reads `done` to the guard and draws as IDLE
in the picture. No HTTP question can tell the two apart, deliberately:
the `attended_at` stamp is this machine's observation of its own keyboard and
never goes on the wire. The sensor setting it here is agterm
(`terminals::agterm`), whose primary signal is *departure* — selecting another
session means you left the one before it — so staging a session to DONE and then
switching tabs is all it takes to mark it read. If the captured frame shows IDLE
where the guard said DONE, make that session emit something fresh: a new
assistant line moves the row's `content_at` past the stamp, and it asks to be
read again.

WHAT THE GUARD REFUSES ON, in the order it checks, most irreversible first:

  * A project name not known to be public. The list is `dash.PUBLISHABLE_PROJECTS`,
    shared with the other capture scripts, and passing `--publishable NAME ...`
    replaces it for one run. Each entry was read off GitHub rather than assumed,
    and a repository can be made private the day after, so it is a claim to
    re-check rather than a fact. Both names a row can draw are checked: the
    project, and the rename a user may have given it.
  * A row synced from another machine, which would put that machine's project
    names in the picture. The check is on the rows rather than on the peer list,
    because a connected peer with no sessions contributes nothing to the frame —
    its device is printed with its session count and refuses nothing.
  * A missing blocked/working/done spread among the local rows.
  * A usage cache that says the bars will not carry a live reading. This one is
    read off disk rather than asked over HTTP: the app's `usage_cache.json` sits
    beside the `config.json` the lib already reads, and it carries both the last
    stored sample (absent means a bar draws a literal `--%`) and `blocked_until`,
    the deadline the endpoint set with its own `Retry-After` on a 429. Until that
    deadline every poll is skipped and the stored sample replayed at 55% opacity
    — "A figure nobody has been able to re-check", as `LimitBar.svelte` puts it,
    and what a dimmed percentage in the hero means. Restarting neither causes nor
    clears it, and waiting the deadline out is the only remedy: the deadline is
    written to disk precisely so a restart cannot reset it, and since 9ae366d a
    new process suppresses its opening poll outright whenever the stored reading
    is younger than the poll interval — a gate named `opening_poll_is_redundant`
    in `usage_limits.rs`, the interval being 600s by default — so the
    deploy-and-restart cycle this staging prescribes costs at most one request
    between restarts. A dim from an expired OAuth token, a dropped network, or a
    429 that carried no retry deadline at all leaves no trace in that file, so
    this catches the common cause and not every cause.

THE PROMPT TEXT IS PUBLISHED TOO, and no list can vet prose. Outside compact
view a row draws its task line, so the hero carries each session's real prompt
into the README — the one figure that does. What the guard can check is names,
so what it does with the prose is print it: every row's text is on stdout before
the shutter and again after it. Read it. The roster's `label` is the exact string
on screen only for the blocked and the error rows; for the others the widget
draws `original_prompt ?? label` (`displayLabel`, `src/lib/types.ts`) and the
roster carries no `original_prompt`, so what is printed for a working or a
finished row is that row's current label rather than the sentence in the picture.

THE GUARD RUNS TWICE, before the window is raised and again after the shutter,
because a real session can change state in the second in between, and the second
pass is what turns that into a refusal rather than a committed frame nobody
re-examines. That second pass runs after the PNG has been written, so a refusal
there is about a file that already exists on disk. Once the figure is committed,
restore it with `git checkout` rather than committing the new one; until then —
and `docs/screenshots/hero-macos.png` has never been captured, so the first run
is this case — the leftover is an untracked PNG to delete, which the manifest
gate `check-figures.py` rejects for as long as it sits there. Either way
the `--probe` flag avoids the question entirely: it writes to `tmp/` and cannot
touch the committed frame at all, which is what to use while the staging is
still moving.

STAGING, every item the operator's and none of it the script's:

  * Deploy the current build, so the frame matches the working tree rather than
    the installed release.
  * Stage one session genuinely blocked on its user, one mid-turn, one finished.
    The hook does not fire on PostToolUse, so a session busy inside a long turn
    does not refresh its row: the working row has to be one that is mid-turn now.
  * Keep every session in a public repository. The guard refuses on the names it
    can read and prints the prompt text it cannot vet.
  * Turn `sync.listen` and `restore_sessions` off, and restart. Both are
    start-only settings the config watcher does not hot-reload — a config edit
    plus a restart before the run, which is why this script cannot write them for
    you the way the compact-mode script writes `compact_mode`. Sync listening off
    keeps a peer's rows, and that machine's project names, out of frame. Session
    restore off is what makes the widget start empty, so only the staged sessions
    get rows instead of every idle session agterm can vouch for.

ONE THING TO CHECK BY EYE BEFORE COMMITTING THE FRAME, and nothing can answer it
for you: the BLOCK pill is at full strength. It pulses between opacity 1 and 0.45
on a 1.6s cycle (`SessionItem.svelte`), nothing can pause it, and the shutter
catches whatever phase it catches — so a washed-out pill is a re-run, not a
setting. The usage bars are the other thing worth a glance, and they no longer
need one: what would empty them or dim them is the fourth guard above.

The `--force` switch skips every guard, for the run where the frame is right and
the roster disagrees, and `--probe` sends the frame to `tmp/`. All three flags
are declared and `allow_abbrev=False` is set, which is this file's version of the
sibling's `[CmdletBinding()]`: an undeclared or half-typed flag has to fail
loudly rather than be swallowed, because the run it silently changes is the one
that overwrites a committed frame.

THE ONE THING IT LEAVES CHANGED is the widget being on screen, which is the
subject of the frame and so has nothing to be restored to. Hiding it with the
lib's `dash.widget_hidden()` would be exactly wrong here — that is for the macOS
captures whose subject is some other window (history-window, terminal-tabs,
work-intensity), and compact-mode-macos, whose subject is the widget too, skips
it for the same reason. Nothing else is touched: no config is written and no
window is resized, so a failure partway through leaves the machine as it was
found.
"""
import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
import dashboard as dash  # noqa: E402 — importable only once its directory is on the path

FIGURE = "hero-macos"

# The main window's configured title (`tauri.conf.json`), matched exactly rather
# than by substring. Every window this app owns reports the same
# CGWindowOwnerName, so the title is the whole discriminator, and the history
# window's title is a session's display name — any string a user has renamed a
# row to. An exact match cannot collide with one; `title_contains="Claude"`
# could. The widget is undecorated, so the title is never drawn: it is still
# reported by CoreGraphics, which is what makes matching on it work at all.
WINDOW_TITLE = "Claude Code Dashboard"

# Longer than the lib's default, and the same wait the Windows sibling takes
# after its show: the widget animates its rows in, and a window revealed from
# the tray is on screen before it has finished drawing them.
SETTLE_MS = 1200

SPREAD = ("blocked", "working", "done")

# The app's own record of the last usage reading and of any deadline the endpoint
# set, in the app data dir the lib already resolves for `config.json`.
USAGE_CACHE = dash.APP_DATA / "usage_cache.json"




def describe(row: dict) -> str:
    """One row as the frame will publish it: the name drawn, where it runs, its badge, its text."""
    where = row.get("device") or ("this machine" if row.get("local") else "?")
    name = row.get("display_name") or row.get("project") or row.get("id", "?")
    return f"{name}@{where} {row.get('status')}: {(row.get('label') or '').strip()!r}"


def assert_frame_worthy(when: str, public: tuple[str, ...]) -> None:
    """Refuse unless the roster describes the picture this figure has to be, and print the prose it cannot judge."""
    roster = dash.agents()
    rows = roster.get("agents", [])
    print(f"Rows {when}:" + ("".join(f"\n  {describe(r)}" for r in rows) or " (none)"))
    for peer in roster.get("peers") or []:
        print(f"  peer {peer.get('device')}: {peer.get('sessions')} sessions, last push {round((peer.get('last_seen_age_ms') or 0) / 1000)}s ago")

    dash.assert_publishable([n for r in rows for n in dash.names_in_frame(r)], allowed=public, where=f"the frame {when}")

    remote = [r for r in rows if not r.get("local")]
    if remote:
        synced = ", ".join(sorted(f"{r.get('device')}/{r.get('project')}" for r in remote))
        raise dash.CaptureError(f"Synced from another machine {when}: {synced}. Those rows put that machine's project names in the picture — turn sync listening off for this frame (`sync.listen` is start-only, so it is a config edit and a restart), or pass --force.")

    local = [r for r in rows if r.get("local")]
    missing = [status for status in SPREAD if status not in {r.get("status") for r in local}]
    if missing:
        raise dash.CaptureError(f"The hero needs one local row in each of blocked/working/done; missing {when}: {', '.join(missing)}. Stage them (see this script's header, and screenshots.json) or pass --force.")


def assert_usage_bars_live(when: str) -> None:
    """Refuse when the app's own usage cache says a bar will draw `--%` or a dimmed figure.

    Both facts are on disk because both have to outlive a restart: the stored
    sample is what the bars replay instead of collapsing to `--%`, and the
    deadline in `blocked_until` is the endpoint's own `Retry-After`, which a
    restart cannot reset.
    """
    try:
        cache = json.loads(USAGE_CACHE.read_text(encoding="utf-8"))
    except FileNotFoundError as e:
        raise dash.CaptureError(f"There is no {USAGE_CACHE.name} in {USAGE_CACHE.parent}, so the app has never stored a usage reading and both bars draw `--%`. Leave the dashboard running until a poll succeeds, or pass --force.") from e
    except (OSError, ValueError) as e:
        raise dash.CaptureError(f"{USAGE_CACHE} could not be read ({e}), so what the usage bars will draw is unknown. Pass --force to photograph anyway.") from e

    empty = [bucket for bucket in ("five_hour", "seven_day") if not cache.get(bucket)]
    if empty:
        raise dash.CaptureError(f"The usage cache holds no {' and no '.join(empty)} reading {when}, so that bar draws a literal `--%`. Leave the dashboard running until a poll succeeds, or pass --force.")

    remaining_s = round(((cache.get("blocked_until") or 0) - time.time() * 1000) / 1000)
    if remaining_s > 0:
        raise dash.CaptureError(f"The usage endpoint asked to be left alone for another {remaining_s}s {when}: every poll until that deadline is skipped and the stored sample replayed at 55% opacity, so the percentage in the frame would be dimmed. Wait it out — restarting neither causes nor clears it — or pass --force.")


def main() -> int:
    ap = argparse.ArgumentParser(description="Capture the macOS hero frame: the widget with a blocked, a working and a finished row.", allow_abbrev=False)
    ap.add_argument("--publishable", nargs="+", metavar="NAME", default=list(dash.PUBLISHABLE_PROJECTS), help="project names allowed on screen; replaces the built-in list rather than extending it")
    ap.add_argument("--probe", action="store_true", help="write to tmp/ instead of the committed frame, for checking the staging")
    ap.add_argument("--force", action="store_true", help="photograph whatever is on screen: skip the public-project, synced-row, blocked/working/done and usage-reading guards")
    args = ap.parse_args()
    public = tuple(sorted({n.strip() for n in args.publishable if n.strip()}))
    out = dash.probe_path(FIGURE) if args.probe else dash.shot_path(FIGURE)

    try:
        if not args.force:
            assert_frame_worthy("before the shot", public)
            assert_usage_bars_live("before the shot")

        # The widget may be hidden to the tray, and a hidden window is absent
        # from the on-screen window list entirely — `find_window` would refuse
        # with "no window at all for that owner" rather than photograph nothing.
        # Raising it is also all the framing this needs: it is always-on-top, so
        # nothing can be covering it.
        dash.window(action="show")
        # `hairline=True` because the widget is an undecorated window: it has no
        # edge of its own, so on a dark page — and the README is rendered on
        # github.com, which has one and which no stylesheet of ours reaches — the
        # picture has no boundary. See `dash.add_hairline`; a decorated window
        # already carries the OS's own stroke and must not be given a second.
        dash.shot(dash.APP, out, title=WINDOW_TITLE, settle_ms=SETTLE_MS, hairline=True)

        if not args.force:
            assert_frame_worthy("after the shot", public)
            assert_usage_bars_live("after the shot")
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
