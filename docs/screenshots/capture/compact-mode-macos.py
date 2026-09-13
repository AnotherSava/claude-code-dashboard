#!/usr/bin/env python3
"""macOS variant of the `compact-mode` figure.

Compact view is a config setting rather than a window state, so this script
writes it, captures, and puts it back. The write belongs to `dash.config_value`
and is not re-implemented here: it writes the file whole and then touches it past
the watcher's 150ms debounce (a truncate-then-write hands the watcher a
half-written file, and every setting silently falls back to its default), and it
restores the previous value in a `finally`, so a run that fails partway cannot
leave the widget in a mode the user never chose.

A KEY THAT WAS ABSENT COMES BACK ABSENT, which matters here because `compact_mode`
usually is absent. The deploy this figure's staging opens with rewrites the whole
of `config.json` from the repo's `config/local.json`, and that file carries only
the handful of keys this machine overrides — so on the manifest's own happy path
the key is missing when the script starts. Putting it back as `null` would fail the
parse for the whole file rather than for one setting, because the Rust side takes
its defaults from `#[serde(default)]`, which fills a missing field and rejects a
null one; the sync token, the device name and the window size would all fall back
at once, silently. Removing the key instead is `dash.config_value`'s job, which is
why nothing here reads or writes `config.json` directly.

THE GUARD IS THE INVERSE OF THE WINDOWS SIBLING'S, deliberately. The script for
compact-mode-windows refuses while a peer is connected, so that frame carries no
device pill — a peer's rows would put the other machine's project names in the
picture. This is the variant that shows sync, so it refuses when *no* row is
synced from another machine: a capture with every row local would be a second
photograph of the Windows figure, missing the pills its caption points at. It
refuses when every row is remote for the mirror-image reason, and it refuses on a
peer whose last push is old enough that its rows are coasting rather than live.
The two variants are read as one figure, so neither half re-decides that pairing.

EVERY SESSION ROW IN FRAME MUST BE A PUBLIC REPOSITORY. A row's label is its real
prompt, so a private project's task text would be published verbatim — and while
compact view suppresses the label line, it still draws the project name, which is
enough on its own. The frame this replaces carries a row for a private project,
which is the whole reason it is being re-shot, so the check is not hypothetical.

The check reads the roster's `agents` array, so it covers the session rows and
nothing else. ONE THING ON SCREEN IS OUTSIDE ITS REACH: the pending-start
approvals panel the widget draws above the session list. It prints the requesting
device and project as one `<device>/<project>` string, and under it the project's
full directory path on that machine. Those requests reach the frontend over the
Tauri command `get_start_approvals` and appear on no HTTP route, so no request
this script can make would ever see one. This is the sync-on variant, so a peer
agent asking to start a session here during the shot is a live case rather than a
hypothetical: answer or dismiss any pending request before the shutter, and read
the top of the frame by eye before committing it.

The allowed names are `dash.PUBLISHABLE_PROJECTS`, which `--publishable` replaces, and the
operator owns that list. It lives in the lib because three scripts enforce the
same rule and a copy per script is three lists to forget to update. Each name on
it was read off the account as a public repository on the day it was written, so
a repository made private the day after would still be waved through — the list is
a claim to re-check rather than a fact. It is deliberately not a live query
against GitHub: that would put the refusal behind a network call and an auth state
whose failure mode is to allow, whereas a list in the repository is reviewable in
a diff and extending it is a deliberate act.

Two things make a name miss the list for an innocent reason. The machines name the
same checkout differently — bga-assistant's row id is `assistant` on the Windows
box, because a row id is the directory basename while `projects_root` is unset —
and a renamed row draws its rename instead of its id. Both are checked, both are
refused with the name printed, and the fix for either is `--publishable` after
confirming what the name points at, not `--force`.

WHAT THE OPERATOR STAGES BY HAND: a deployed current build; sessions live on this
machine and on the other one; and every one of those sessions in a public
repository. The peer's push has to be recent, which the script checks and prints
rather than leaving to the eye — a sleeping machine's rows sit frozen at their
last-pushed status until the 90s TTL drops them, and a frame shot in that window
looks exactly like a live one. Compact view is this script's to set, and the
widget is the subject of the frame, so nothing hides it — `dash.widget_hidden` is
for the frames it would otherwise sit on top of.

The status badges are not guarded, unlike the hero's blocked/working/done spread.
The roster at `/api/agents` is served from `resolved_snapshot` while the widget
draws `display_snapshot`, so a finished row the user has already looked at reads
IDLE on screen and `done` here, and no HTTP question can tell the two apart. That
divide does not matter for this figure, whose subject is the compact layout and
the device pills rather than any particular set of badges.

The corner halo is trimmed inside `dash.capture_window`, and the PNG is converted
to sRGB there as well: `screencapture` tags its output with the display's
wide-gamut profile, under which `check-figures.py` no longer recognises the
traffic lights it identifies a decorated macOS window by. So nothing below runs
either `lib/trim_halo.py` or `sips`. The compact-mode-macos manifest entry still
tells a re-shooter to run the trim by hand, and still carries no `capture.command`
pointing at this script, so whoever follows the manifest never learns this file
exists; both steps predate the lib and are owed a manifest pass covering the whole
macOS set.
"""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))   # the lib sits beside this file, so the insert has to precede the import
import dashboard as dash  # noqa: E402

FIGURE = "compact-mode-macos"
WINDOW_TITLE = "Claude Code Dashboard"   # the widget; History, About and Work intensity are separate titles under the same owner

# How stale a peer's last push may be before the frame is refused. Half the 90s
# TTL a silent device's rows survive for (`sync::REMOTE_TTL_MS`), and a chosen
# fraction of it rather than a measured number: the pusher heartbeats every 30s,
# so a peer that is genuinely awake clears this with a cycle to spare, while one
# that went to sleep during the staging is caught before its frozen rows reach the
# PNG.
STALE_PUSH_MS = 45_000




def describe(row: dict) -> str:
    """One row as a line of run output, carrying the push age the pill's honesty rests on."""
    where = row.get("device") or ("this machine" if row.get("local") else "?")
    age = row.get("last_seen_age_ms")
    pushed = f" pushed {round(age / 1000)}s ago," if age is not None else ""
    return f"{row.get('project')}@{where},{pushed} {row.get('status')}" + (f" as {row['display_name']!r}" if row.get("display_name") else "")


def sync_hint(roster: dict) -> str:
    """How to get a peer's rows on screen, given what the roster already says."""
    if not roster.get("sync_listening"):
        return "This dashboard is not listening for pushes: turn sync listening on in config/local.json, redeploy and restart — it is read at startup only."
    peers = roster.get("peers", [])
    if not peers:
        return "No peer has pushed at all, so the other machine's dashboard is down, asleep or off the tailnet."
    return "Connected with nothing to send: " + ", ".join(f"{p.get('device')} ({p.get('sessions')} sessions, last push {round(p.get('last_seen_age_ms', 0) / 1000)}s ago)" for p in peers) + ". Start a session over there and wait a heartbeat."


def assert_frame_worthy(when: str, public) -> None:
    """Refuse unless the roster describes the picture this figure has to be.

    Only `agents` is examined. The roster's `registry_only` array is a read-path
    union of Claude Code's own live-session list and never becomes a dashboard
    row, so none of it reaches the frame. The approvals panel does reach the frame
    and is on no HTTP route at all, which is why the module docstring hands it to
    the operator as an eye-check instead.
    """
    roster = dash.agents()
    rows = roster.get("agents", [])
    print(f"Rows {when}: " + (", ".join(describe(r) for r in rows) or "(none)"))

    if not rows:
        raise dash.CaptureError(f"The widget has no rows {when}, so there is nothing to photograph.")

    dash.assert_publishable([n for r in rows for n in dash.names_in_frame(r)], allowed=public, where=f"the frame {when}")

    remote = [r for r in rows if not r.get("local")]
    if not remote:
        raise dash.CaptureError(f"No row is synced from another machine {when}, so the frame would carry no device pill — which is the one thing this variant shows that the Windows one does not. {sync_hint(roster)} Or pass --force.")

    if not dash.local_rows(roster):
        raise dash.CaptureError(f"Every row is synced from another machine {when}, so nothing in frame is local and every row would wear a device pill. The caption points at 'device pills on the three rows synced from another machine', which says nothing when the rest carry them too. Start a session on this machine, or pass --force.")

    stale = [r for r in remote if r.get("last_seen_age_ms") is not None and r["last_seen_age_ms"] > STALE_PUSH_MS]
    if stale:
        raise dash.CaptureError(f"A peer has not pushed for over {STALE_PUSH_MS // 1000}s {when}: {', '.join(describe(r) for r in stale)}. Those rows are frozen at whatever they last said and drop out at the 90s TTL, so the frame would show a machine that is asleep as though it were working. Wake it and wait for a heartbeat, or pass --force.")


def main() -> int:
    # allow_abbrev=False for the reason lib/dashboard.ps1 records for
    # [CmdletBinding()]: a flag that is accepted, ignored and run past is how a
    # probe once took the machine over and overwrote a committed frame.
    parser = argparse.ArgumentParser(allow_abbrev=False, description="Capture the macOS compact-mode figure: the widget with its rows reduced to name and badge, device pills included.")
    parser.add_argument("--publishable", nargs="+", metavar="NAME", default=list(dash.PUBLISHABLE_PROJECTS), help="project names allowed on screen; replaces the lib's list rather than extending it")
    parser.add_argument("--probe", action="store_true", help="write to tmp/ instead of the committed frame, for iterating on the staging")
    parser.add_argument("--force", action="store_true", help="photograph whatever is on screen: skip the public-project, local-row, device-pill and push-freshness guards")
    args = parser.parse_args()

    public = tuple(sorted({n.strip() for n in args.publishable if n.strip()}))   # sorted so a refusal prints the same list twice running
    out = dash.probe_path(FIGURE) if args.probe else dash.shot_path(FIGURE)

    try:
        if not args.force:
            assert_frame_worthy("before the shot", public)
        with dash.config_value("compact_mode", True):
            dash.window(action="show")
            # Compact view re-renders every row shorter and the window re-fits to
            # the new content, so the settle has to cover that resize and not just
            # the reveal.
            # `hairline=True` because the widget is an undecorated window: it has
            # no edge of its own, so on a dark page the picture has no boundary.
            # See `dash.add_hairline` — a decorated window must not get one.
            dash.shot(dash.APP, out, title=WINDOW_TITLE, settle_ms=1200, hairline=True)
            # Second pass, as the hero does: a session can change state or a peer
            # can go silent inside the second the shutter takes. This one cannot
            # un-write the file, so it refuses loudly instead — restore the frame
            # with git and stage it again.
            if not args.force:
                assert_frame_worthy("after the shot", public)
    except dash.CaptureError as e:
        print(f"refused: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
