#!/usr/bin/env python3
"""Shared helpers for the capture scripts: the macOS ones import it, and the Windows ones shell out to its command line.

Import it: `sys.path.insert(0, str(Path(__file__).parent / "lib"))` then
`import dashboard as dash`. The macOS counterpart of `lib/dashboard.ps1` plus
the docs-relevance skill's `window-shot.ps1` and `windows-capture.ps1`, which
are one file here because macOS needs almost none of what the Windows capture
needs.

WHY THIS IS SO MUCH SMALLER THAN THE WINDOWS HALF, since a reader coming from
`window-shot.ps1` will expect several hundred lines and find sixty. That file is long because
Windows fights the capture: the thread has to be made PerMonitorV2 aware or every
geometry call reads back divided by the monitor scale; `GetWindowRect` includes
about 7px of invisible resize margin that has to be subtracted via
`DWMWA_EXTENDED_FRAME_BOUNDS`; and there is no API that returns a window with its
alpha, so the Alpha method photographs the window twice over two backdrops and
solves the per-pixel alpha. macOS has one call that does all of it —
`screencapture -l<id> -o` writes the window at device resolution with real
per-pixel alpha and no shadow. The remaining work is finding the id.

RUN THESE FROM A TERMINAL THAT HOLDS SCREEN RECORDING PERMISSION. Both halves
need it: without it `screencapture` writes a picture of the desktop wallpaper
rather than the window, and CoreGraphics blanks every window title so the match
fails first. The failure is loud in the second case and silent in the first,
which is why `capture_window` checks the size it got back.

THE ORIGIN HEADER IS OMITTED DELIBERATELY on every request below. `/api/window`
and `/api/agents` run the CSRF guard, which accepts a request carrying no Origin
and refuses every value one can be set to — including the plausible
`http://127.0.0.1:9077`, which answers 403 `csrf`. Setting it "properly" is the
mistake that looks like a fix.
"""
import argparse
import base64
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from contextlib import contextmanager
from pathlib import Path

from PIL import Image

LIB = Path(__file__).resolve().parent
CAPTURE = LIB.parent
SHOTS = CAPTURE.parent
REPO = SHOTS.parent.parent

APP_DATA = (Path(os.environ["APPDATA"]) if sys.platform == "win32" else Path.home() / "Library" / "Application Support") / "com.anothersava.claude-code-dashboard"
CONFIG = APP_DATA / "config.json"

# The window is on screen before it is finished drawing: the widget animates its
# rows in and the intensity chart animates its bars, so a capture taken the
# instant the API returns catches a half-rendered frame.
SETTLE_MS = 700


class CaptureError(RuntimeError):
    """Anything that should stop a capture rather than commit a wrong picture."""


# Shared capture machinery lives in the docs-relevance skill, not here.
#
# The split is: anything a different project would want unchanged — taking the
# picture, listing windows, trimming a halo, stroking an edge — belongs to the
# skill and is called from it, so a fix reaches every project at once and two
# copies cannot drift. What stays in this directory is only what this project
# knows: which window, which state to stage, which sessions may appear in frame.
# A `~`-relative path is not machine-specific — it resolves the same wherever
# those dotfiles are installed, which is the same condition under which a capture
# runs at all. The skill was named `documentation` until the dotfiles renamed it
# on 2026-09-17; the old path lived on here for three days, and every re-shoot on
# either platform would have thrown `hairline.py is missing` until it was fixed.
SKILL_SCRIPTS = Path.home() / ".claude" / "skills" / "docs-relevance" / "scripts"


def skill_script(name: str) -> Path:
    """A script from the docs-relevance skill, or a refusal that says how to get it."""
    p = SKILL_SCRIPTS / name
    if not p.exists():
        raise CaptureError(f"{name} is missing from {SKILL_SCRIPTS}. The capture scripts call the docs-relevance skill's shared tooling; install the dotfiles (see their README) and run this again.")
    return p


# ---------------------------------------------------------------- the app's API

def dashboard_port() -> int:
    """The port the running app actually listens on, read from its own config.

    Not a constant: `config/local.json` is sometimes pointed at 9078 to keep a
    test instance off the live one, and a capture script that quietly talked to
    the wrong instance would produce a plausible screenshot of the wrong thing.
    """
    try:
        port = json.loads(CONFIG.read_text(encoding="utf-8")).get("server_port")
        return int(port) if port else 9077
    except (OSError, ValueError, TypeError):
        return 9077


def _get(path: str) -> dict:
    url = f"http://127.0.0.1:{dashboard_port()}{path}"
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.URLError as e:
        raise CaptureError(f"GET {path} failed on port {dashboard_port()} ({e}). Is the dashboard running?") from e


def window(**body) -> dict:
    """POST /api/window — show, hide, resize, move, maximize, history, intensity.

    Every documentation frame is a window the app can be asked to open, size and
    place, so a capture script states the window it wants instead of telling a
    human where to click. The API is a control surface and not a capture hook: it
    will not scroll a window or frame a shot, so anything beyond show/size/place
    stays in the per-frame script where it can be read.
    """
    url = f"http://127.0.0.1:{dashboard_port()}/api/window"
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},   # no Origin; see the module docstring
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        # A refusal is a non-2xx, so the reason has to be read out of the body
        # rather than off the parsed response.
        reason = ""
        try:
            reason = json.loads(e.read().decode("utf-8")).get("reason", "")
        except Exception:
            pass
        raise CaptureError(f"The dashboard refused {body.get('action')}: HTTP {e.code}{f' ({reason})' if reason else ''}") from e
    except urllib.error.URLError as e:
        raise CaptureError(f"POST /api/window failed on port {dashboard_port()} ({body.get('action')}): {e}. Is the dashboard running?") from e


def agents() -> dict:
    """GET /api/agents — the roster, for a script that has to check what it is about to photograph."""
    return _get("/api/agents")


def local_rows(roster: dict | None = None) -> list[dict]:
    r = roster if roster is not None else agents()
    return [a for a in r.get("agents", []) if a.get("local")]


def shot_path(figure_id: str) -> Path:
    """docs/screenshots/<figure_id>.png — the committed frame."""
    return SHOTS / f"{figure_id}.png"


def probe_path(name: str) -> Path:
    """tmp/<name>.png — where an iteration goes, so tuning cannot leave a half-tuned picture in the repo."""
    out = REPO / "tmp"
    out.mkdir(exist_ok=True)
    return out / f"{name}.png"


# ------------------------------------------------------------------- the window

def list_windows() -> list[dict]:
    """Every on-screen top-level window, via the docs-relevance skill's `window_list.swift`."""
    try:
        r = subprocess.run(["swift", str(skill_script("window_list.swift"))], capture_output=True, timeout=60)
    except FileNotFoundError as e:
        raise CaptureError("`swift` is not on PATH. Install the Xcode Command Line Tools (`xcode-select --install`).") from e
    except subprocess.TimeoutExpired as e:
        raise CaptureError("window_list.swift did not finish within 60s") from e
    if r.returncode != 0:
        raise CaptureError(f"window_list.swift failed: {r.stderr.decode('utf-8', 'replace').strip()}")
    return json.loads(r.stdout.decode("utf-8"))


def find_window(owner: str, title: str | None = None, title_contains: str | None = None) -> dict:
    """The one on-screen window matching owner and title, or a refusal naming the alternatives.

    Ambiguity is refused rather than resolved by picking the first. The Windows
    half has a `-First` switch for the case where a tie is expected and the newest
    window is the one wanted; nothing here needs it yet, and a silent pick is how
    a capture script photographs the wrong window and nobody notices for a
    release.
    """
    wins = [w for w in list_windows() if w["owner"] == owner]
    if title is not None:
        wins = [w for w in wins if w["title"] == title]
    if title_contains is not None:
        wins = [w for w in wins if title_contains in w["title"]]

    want = f"owner={owner!r}" + (f" title={title!r}" if title is not None else "") + (f" title~{title_contains!r}" if title_contains is not None else "")
    if not wins:
        seen = ", ".join(sorted({f"{w['owner']}:{w['title']}" for w in list_windows() if w["owner"] == owner})) or "(no window at all for that owner)"
        raise CaptureError(f"No on-screen window matches {want}. Windows for that owner: {seen}")
    if len(wins) > 1:
        seen = ", ".join(f"id={w['id']} {w['title']!r} {int(w['w'])}x{int(w['h'])}" for w in wins)
        raise CaptureError(f"{len(wins)} windows match {want}, so the capture will not guess: {seen}")
    return wins[0]


def capture_window(win: dict, out: Path, trim: bool = True) -> Path:
    """Photograph one window to a PNG at device resolution, with real alpha.

    `-o` drops the compositor's drop shadow, which is the whole reason the corner
    comes out clean: the shadow is what `trim_halo.py` was written to chase off
    the four corners of the frames captured before this script existed, and those
    frames carry a fully transparent one-pixel ring inside the rounded corner as
    the residue of trimming it. Measured on a fresh `-o` capture the corner ramps
    0 -> 150 -> 255 with no hole.

    `trim_halo` is still run, and unconditionally rather than only when a halo is
    suspected: it is a flood fill from each corner bounded to a 48px box over
    faint-or-black pixels, so on a frame that has no halo it clears nothing and
    says so. Running it always is what keeps the macOS frames from depending on
    the operator having remembered — the manifest used to say a macOS re-shoot
    must run it by hand, and that is exactly the kind of step that gets skipped.
    """
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()
    r = subprocess.run(["screencapture", f"-l{win['id']}", "-o", "-x", "-t", "png", str(out)], capture_output=True, timeout=60)
    if r.returncode != 0 or not out.exists():
        raise CaptureError(f"screencapture failed for window {win['id']} ({win['title']!r}): {r.stderr.decode('utf-8', 'replace').strip()}")

    # Screen Recording denied does not fail: screencapture writes a picture of
    # the desktop instead, at the display's size rather than the window's. So the
    # size is checked against what CoreGraphics said the window measures.
    from PIL import Image
    with Image.open(out) as im:
        w, h = im.size
    scale = round(w / win["w"]) if win["w"] else 0
    if scale not in (1, 2) or abs(h - win["h"] * scale) > 2 * scale:
        raise CaptureError(
            f"{out.name} is {w}x{h}, which is not window {win['id']} ({int(win['w'])}x{int(win['h'])} logical) at 1x or 2x. "
            "The usual cause is Screen Recording permission: grant it to this terminal in "
            "System Settings > Privacy & Security > Screen Recording, then run it again."
        )

    keep_raw(out)
    if trim:
        subprocess.run([sys.executable, str(LIB / "trim_halo.py"), str(out)], check=True, timeout=120)
    to_srgb(out)
    return out


def keep_raw(out: Path) -> Path | None:
    """Put the untouched capture in `tmp/raw/` before anything rewrites it.

    Every step after this one — the halo trim, the sRGB conversion, the hairline —
    edits the file in place, so without this the capture is destroyed by its own
    post-processing and the only way to try a different treatment is to take the
    shot again. That is a bad trade whatever it costs, and here it costs a great
    deal: the window has to be on screen, which it is not when a full-screen app
    is in front, and several of these frames need session states that can only be
    staged by hand. Tuning a border is post-processing and should never have
    needed the app running at all.

    It really was the difference between doing the work and not: the border was
    reworked three times, and the run that finally settled it replayed a raw left
    over from an unrelated probe, because by then the window was unreachable.

    `tmp/` is scratch and gitignored, so this commits nothing and is safe to
    delete. Failures are warned about rather than raised — a missing archive
    copy must not lose a capture that succeeded.
    """
    try:
        raw = REPO / "tmp" / "raw" / f"{out.stem}.raw.png"
        raw.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(out, raw)
        return raw
    except OSError as e:
        print(f"WARNING: could not archive the raw capture of {out.name} ({e}); post-processing it will need a re-shoot.", file=sys.stderr)
        return None


SRGB_PROFILE = Path("/System/Library/ColorSync/Profiles/sRGB Profile.icc")


def to_srgb(path: Path) -> None:
    """Convert a capture to sRGB, which is the space the committed frames are in.

    `screencapture` tags its output with the DISPLAY's profile — "Color LCD" on
    this machine, a wide-gamut one. The picture looks right, and every stored
    number is wrong for anything that reads the pixels: the same window chrome
    that is (241, 0, 27) in sRGB is stored as (221, 47, 44) in the wider space.
    It looks like a desaturated version of itself, because that is exactly what
    it is — the same colour needs less extreme coordinates when the coordinates
    reach further.

    That silently broke a real check. `check-figures.py` identifies a macOS frame
    by its three traffic lights over a saturation floor of 0.55, and the green
    light drops from 1.00 to 0.74 across the conversion, so a wide-gamut capture
    of a plainly-decorated macOS window is reported as one whose platform cannot
    be confirmed. Since that check is the one that catches a frame filed under
    the wrong OS — the defect this whole rework started from — leaving every new
    macOS frame outside it would have quietly retired it.

    So this runs LAST, after the trim, and the conversion is verified rather than
    assumed: `sips` is the only tool here that writes an ICC profile, and a
    Pillow save that lost `info["icc_profile"]` would otherwise leave an untagged
    file that everything downstream would *guess* was sRGB.
    """
    if not SRGB_PROFILE.exists():
        print(f"WARNING: {SRGB_PROFILE.name} is missing, so {path.name} keeps the display's colour profile. Its pixels will not match the other committed frames.", file=sys.stderr)
        return
    tmp = path.with_suffix(".srgb-tmp.png")
    r = subprocess.run(["sips", "--matchTo", str(SRGB_PROFILE), str(path), "--out", str(tmp)], capture_output=True, timeout=120)
    if r.returncode != 0 or not tmp.exists():
        tmp.unlink(missing_ok=True)
        raise CaptureError(f"sips could not convert {path.name} to sRGB: {r.stderr.decode('utf-8', 'replace').strip()}")
    tmp.replace(path)


def shot(owner: str, out: Path, title: str | None = None, title_contains: str | None = None, settle_ms: int = SETTLE_MS, trim: bool = True, hairline: bool = False) -> Path:
    time.sleep(settle_ms / 1000)
    win = find_window(owner, title=title, title_contains=title_contains)
    path = capture_window(win, out, trim=trim)
    # Declared at the call site rather than decided here: only the undecorated
    # widget needs one, and a decorated window must not be given a second edge.
    if hairline:
        add_hairline(path)
    print(f"{path.relative_to(REPO)} <- {owner} {win['title']!r} ({int(win['w'])}x{int(win['h'])} logical)")
    return path


def add_hairline(path: Path) -> None:
    """Give a capture the edge it has none of — see `lib/hairline.py`.

    Shelled out to rather than implemented here, and the same way
    `capture_window` runs `trim_halo.py`: one implementation for every project,
    in the skill that owns the machinery. The Windows half no longer calls it: a
    Windows capture's frame is drawn afresh from DWM's own model by the skill's
    `winframe.py`, because a captured Windows border is mixed with the shadow
    behind it and cannot be kept or stroked over.

    It decides for itself whether the frame needs one: most captures arrive with
    the OS's own border and are left alone.
    """
    # --require turns a SKIP into a failure. `hairline.py` exits 0 both when it
    # strokes a frame and when it decides the frame already has an edge, so without
    # this a false positive from its gate ships an unbordered frame and reports
    # success. This function is only called when a border is wanted.
    r = subprocess.run([sys.executable, str(skill_script("hairline.py")), "--require", str(path)], capture_output=True, timeout=300)
    sys.stdout.write(r.stdout.decode("utf-8", "replace"))
    if r.returncode != 0:
        raise CaptureError(f"hairline.py failed on {path.name}: {r.stderr.decode('utf-8', 'replace').strip()}")
    # Re-tag, because the colour marker `to_srgb` leaves is a PNG `sRGB` CHUNK
    # rather than an embedded ICC profile: `sips` writes and reports it, and
    # Pillow can neither see it (`info["icc_profile"]` is empty) nor write it
    # back, so any save through Pillow drops it and the frame lands untagged.
    # Cheaper and more honest than teaching the shared script a macOS-only trick,
    # and it keeps `sips` as the last thing to touch the file.
    to_srgb(path)


APP = "Claude Code Dashboard"   # the owner name CoreGraphics reports for this app's windows


@contextmanager
def widget_hidden():
    """Hide the widget for the duration, whatever happens.

    For every frame that is NOT the widget itself. The widget is always-on-top,
    so raising the target window does not get it out from under — it stays above
    whatever has focus, and a capture that reads pixels off the screen
    photographs it sitting inside the frame. It goes back afterwards including on
    a failed capture, because a capture script must not leave the user's widget
    hidden.
    """
    window(action="hide")
    time.sleep(0.4)
    try:
        yield
    finally:
        window(action="show")


# ---------------------------------------------------------------------- config

@contextmanager
def config_value(key: str, value):
    """Set one config key for the duration and put the old one back.

    The file is written WHOLE and then touched, rather than truncated in place.
    The config watcher reads on the first notify event and then debounces for
    150ms, so a truncate-then-write hands it a half-written file, every setting
    silently falls back to its default, and the real write is swallowed by the
    debounce. Writing the complete file, waiting past the debounce and touching
    it gives the watcher a second event against a file that is already correct.

    The restore is in `finally`: a capture that fails partway through must not
    leave the user's widget in a mode they did not choose.

    A KEY THAT WAS ABSENT IS RESTORED BY REMOVING IT, not by writing `null`, and
    the difference is the whole config rather than one setting. The Rust side
    takes its defaults from `#[serde(default)]`, which fills in a MISSING field
    and does not accept a null one, so putting the key back as `null` fails the
    parse for the entire file and every setting falls back at once — a widget
    that comes back at the wrong size with sync off, from a script whose only job
    was to flip one flag back. Absence is reachable in ordinary use: `deploy`
    rewrites `config.json` from the repo's `config/local.json`, which carries
    only the handful of keys that machine overrides.
    """
    if not CONFIG.exists():
        raise CaptureError(f"No config at {CONFIG} — is the dashboard installed?")
    loaded = json.loads(CONFIG.read_text(encoding="utf-8"))
    was, was_present = loaded.get(key), key in loaded

    def write(v, present=True):
        cfg = json.loads(CONFIG.read_text(encoding="utf-8"))
        if present:
            cfg[key] = v
        else:
            cfg.pop(key, None)
        tmp = CONFIG.with_suffix(".json.capture-tmp")
        tmp.write_text(json.dumps(cfg, indent=2) + "\n", encoding="utf-8")
        tmp.replace(CONFIG)
        time.sleep(0.7)                       # clear the watcher's 150ms debounce
        os.utime(CONFIG, None)
        time.sleep(1.5)

    write(value)
    try:
        yield
    finally:
        write(was, was_present)


# -------------------------------------------------------------------- fixtures
#
# A fixture is a figure's rows written down as the input that produces them. A
# local row is the hook events a session would send, replayed through the
# dashboard's own `/api/event`, so the production classifier and state machine
# build it. A row with a `device` is pushed to the dashboard's sync listener as
# that peer, the way the peer's own dashboard would push it. Either way the frame
# is the real widget rendering a state it reaches from real input, and every
# string in it was written for the frame, so no live session is arranged and no
# private prompt can appear.
#
# A row's `age_min` is how long it has been in its state when the shutter fires.
# A local row can only start its state when its events arrive, so it is replayed
# that many minutes before the others and `fixture_up` waits out the difference;
# a pushed row carries its own timestamps, so it is simply dated back.
#
# `fixture_up` restarts the app on a capture config and replays the rows;
# `fixture_down` restarts it on the original config, restored byte for byte, and
# removes what the replay left in the app's stores. What `fixture_down` needs is
# written to FIXTURE_STATE before anything changes, so a run that dies in between
# is recovered by running `fixture-down` on its own.

FIXTURE_STATE = REPO / "tmp" / "fixture-state.json"
FIXTURE_SCRATCH = REPO / "tmp" / "fixture-run"
HISTORY = APP_DATA / "prompt_history.json"
REMOTE_HISTORY = APP_DATA / "remote_history"
SESSION_IDS = APP_DATA / "session_chat_ids.json"
APP_PROCESS = "claude-code-dashboard"
_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def load_fixture(path: Path) -> dict:
    fx = json.loads(Path(path).read_text(encoding="utf-8"))
    if not fx.get("rows"):
        raise CaptureError(f"{path} has no rows.")
    return fx


def _session_id(row: dict) -> str:
    return f"fixture-{row['project']}"


def capture_config(original: dict, fx: dict) -> dict:
    """The config a fixture capture runs on: the user's own, minus whatever would
    put something other than the fixture in frame or send its rows anywhere.

    - `restore_sessions` off: live sessions would get their rows back from their
      tab titles when the app starts.
    - no `sync.peers`, so the pusher sends the fixture's rows to nobody, and
      `sync.listen` off unless the fixture pushes rows of its own: a real peer's
      rows would otherwise join the widget. When it is on, `check_shown` still
      refuses a frame a real peer's push has reached.
    - `instruction_canary_enabled` off: a replayed row would read amber, the "not
      yet seen" canary colour, which the downloadable build never shows because it
      ships the canary off.
    - `high_alert` off and no `notifications`: a replayed BLOCK or DONE could ping
      the phone. Telegram has no on/off switch, so its block is left out.

    The fixture's own `config` goes on top, for the presentation its frame needs.
    """
    cfg = json.loads(json.dumps(original))
    cfg.update({"restore_sessions": False, "instruction_canary_enabled": False, "high_alert": False})
    cfg["sync"] = {**cfg.get("sync", {}), "listen": any(r.get("device") for r in fx["rows"]), "peers": []}
    cfg.pop("notifications", None)
    cfg.update(fx.get("config", {}))
    return cfg


def post_event(event: str, payload: dict) -> None:
    """POST /api/event with a hook payload, the way `integrations/claude_hook.py` sends one.

    `console_pids` is empty on purpose: it is what the dashboard titles a terminal
    tab through, and a fixture row has no tab, so none of the user's is retitled.
    """
    body = {"client": "claude", "event": event, "payload": {"hook_event_name": event, **payload}, "console_pids": []}
    req = urllib.request.Request(f"http://127.0.0.1:{dashboard_port()}/api/event", data=json.dumps(body).encode("utf-8"), headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            r.read()
    except urllib.error.URLError as e:
        raise CaptureError(f"POST /api/event ({event}) failed on port {dashboard_port()}: {e}. Is the dashboard running?") from e


def _row_payload(fx: dict, row: dict, scratch: Path) -> dict:
    """What every event of one row carries: its session, its directory, and a
    transcript holding its token count when the fixture gives one.

    The directory only has to derive the row's name, so it sits in the scratch
    tree, where no real session lives. The transcript is one assistant entry,
    which the dashboard's transcript watcher reads for the token column.
    """
    cwd = scratch / "projects" / row["project"]
    cwd.mkdir(parents=True, exist_ok=True)
    payload = {"session_id": _session_id(row), "cwd": str(cwd)}
    if row.get("input_tokens"):
        text = next((p.get("last_assistant_message", "") for e, p in reversed(row["events"]) if e == "Stop"), "")
        usage = {"input_tokens": 1000, "cache_creation_input_tokens": 0, "cache_read_input_tokens": row["input_tokens"] - 1000, "output_tokens": 500}
        entry = {"type": "assistant", "isSidechain": False, "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime()), "message": {"model": fx.get("model", "claude-opus-5-5"), "role": "assistant", "content": [{"type": "text", "text": text}], "usage": usage}}
        transcript = scratch / "transcripts" / f"{row['project']}.jsonl"
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_bytes((json.dumps(entry) + "\n").encode("utf-8"))
        payload["transcript_path"] = str(transcript)
    return payload


def _age_ms(row: dict) -> int:
    return int(row.get("age_min", 0) * 60_000)


def replay(fx: dict, scratch: Path) -> None:
    """Replay the local rows oldest first, each at the moment that leaves it
    `age_min` old when the last one lands, then push the peers' rows.

    The peers go last because a pushed row lasts only REMOTE_TTL_MS (90s) after
    its push, so it has to arrive just before the shutter.
    """
    local = sorted((r for r in fx["rows"] if not r.get("device")), key=_age_ms, reverse=True)
    lead = max((_age_ms(r) for r in local), default=0)
    if lead:
        print(f"fixture: replaying over {lead / 60_000:g} min so the rows' timers differ", flush=True)
    start = time.monotonic()
    for row in local:
        time.sleep(max(0.0, start + (lead - _age_ms(row)) / 1000 - time.monotonic()))
        base = _row_payload(fx, row, scratch)
        for event, payload in row["events"]:
            post_event(event, {**base, **payload})
    time.sleep(max(0.0, start + lead / 1000 - time.monotonic()))
    push_peer_rows(fx)


def push_peer_rows(fx: dict) -> None:
    """Push each device's rows to this dashboard's sync listener, as that peer's
    own dashboard would.

    A pushed row is a whole session record, so its clock is dated back by
    `age_min` rather than waited for. Its `dialog_tip` is 0, so the receiver asks
    the pretend peer for no content; the push is all there is.
    """
    sync = json.loads(CONFIG.read_text(encoding="utf-8")).get("sync", {})
    now = int(time.time() * 1000)
    devices: dict[str, list[dict]] = {}
    for row in fx["rows"]:
        if row.get("device"):
            devices.setdefault(row["device"], []).append(row)
    for device, rows in devices.items():
        sessions = []
        for r in rows:
            entered = now - _age_ms(r)
            session = {"id": r["project"], "status": r["expect"], "label": r["prompt"], "original_prompt": r["prompt"], "task_started_at": entered, "dialog": [], "source": "claude", "model": fx.get("model", "claude-opus-5-5"), "input_tokens": r.get("input_tokens"), "updated": now, "state_entered_at": entered, "working_accumulated_ms": _age_ms(r) if r["expect"] == "working" else 0}
            sessions.append({"session": session, "dialog_tip": 0})
        body = {"device_name": device, "listen_port": sync.get("listen_port", 9078), "sessions": sessions}
        req = urllib.request.Request(f"http://127.0.0.1:{sync.get('listen_port', 9078)}/api/sync", data=json.dumps(body).encode("utf-8"), headers={"Content-Type": "application/json", "Authorization": f"Bearer {sync.get('token', '')}"}, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                resp.read()
        except urllib.error.URLError as e:
            raise CaptureError(f"Pushing {device}'s rows to the sync listener failed: {e}. The listener needs sync.token set in the config.") from e


def check_shown(fx: dict, wait_s: float = 0) -> None:
    """Refuse unless the dashboard holds exactly the fixture's rows, each in the
    status the fixture expects.

    Exactly, because this is also the publishing guard: a live session that acted
    since the restart puts its own row, with its real prompt, into the frame.
    """
    want = {(f"{r['device']}/{r['project']}" if r.get("device") else r["project"]): r["expect"] for r in fx["rows"]}
    deadline = time.monotonic() + wait_s
    while True:
        got = {a["id"]: a.get("status") for a in agents().get("agents", [])}
        if got == want:
            return
        if time.monotonic() >= deadline:
            raise CaptureError(f"The widget is not showing exactly the fixture: expected {want}, got {got}. A row the fixture does not name belongs to a live session that acted after the restart; wait for it to go quiet and run this again.")
        time.sleep(0.5)


def _app_running() -> bool:
    r = subprocess.run(["tasklist", "/FI", f"IMAGENAME eq {APP_PROCESS}.exe", "/NH"], capture_output=True, text=True, timeout=30, creationflags=_NO_WINDOW)
    return f"{APP_PROCESS}.exe" in r.stdout


def _app_exe() -> str:
    r = subprocess.run(["powershell", "-NoProfile", "-Command", f"(Get-Process '{APP_PROCESS}' -ErrorAction SilentlyContinue).Path"], capture_output=True, text=True, timeout=30, creationflags=_NO_WINDOW)
    paths = [line.strip() for line in r.stdout.splitlines() if line.strip()]
    if len(paths) != 1:
        raise CaptureError(f"Expected one running {APP_PROCESS}, found {len(paths)}. Start the dashboard, or close the extra one, and run this again.")
    return paths[0]


def _stop_app() -> None:
    subprocess.run(["taskkill", "/IM", f"{APP_PROCESS}.exe", "/F"], capture_output=True, timeout=30, creationflags=_NO_WINDOW)
    deadline = time.monotonic() + 15
    while _app_running():
        if time.monotonic() >= deadline:
            raise CaptureError(f"{APP_PROCESS} is still running 15s after taskkill.")
        time.sleep(0.3)


def _start_app(exe: str) -> None:
    os.startfile(exe)
    deadline = time.monotonic() + 30
    while True:
        try:
            agents()
            return
        except CaptureError:
            if time.monotonic() >= deadline:
                raise CaptureError(f"{exe} started but its API did not answer within 30s.")
            time.sleep(0.5)


def _edit_json(path: Path, edit) -> None:
    """Read a JSON object store, apply `edit` to it, and write it back as bytes, so
    no Windows text-mode newline translation reaches the file."""
    data = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    edit(data)
    path.write_bytes((json.dumps(data, indent=2, ensure_ascii=False) + "\n").encode("utf-8"))


def fixture_up(path: Path) -> None:
    """Restart the dashboard on a capture config and show the fixture's rows."""
    if sys.platform != "win32":
        raise CaptureError("Fixture capture restarts the app, and that part is written for Windows only so far.")
    if FIXTURE_STATE.exists():
        raise CaptureError(f"{FIXTURE_STATE} is left from a run that never finished. Run `python {Path(__file__).name} fixture-down` to put the dashboard back first.")
    fx = load_fixture(path)
    exe = _app_exe()
    history = json.loads(HISTORY.read_text(encoding="utf-8")) if HISTORY.exists() else {}
    # A history entry the fixture's row ids already have is put back as it was
    # rather than deleted: the projects are real, so this machine has history for
    # some of them. A pushed device's stored history is kept whole for the same
    # reason; None records a file that did not exist.
    local = [r["project"] for r in fx["rows"] if not r.get("device")]
    devices = {r["device"] for r in fx["rows"] if r.get("device")}
    stored = {d: (base64.b64encode(f.read_bytes()).decode("ascii") if (f := REMOTE_HISTORY / f"{d}.json").exists() else None) for d in devices}
    state = {"fixture": str(Path(path).resolve()), "exe": exe, "config": base64.b64encode(CONFIG.read_bytes()).decode("ascii"), "history": {p: history.get(p) for p in local}, "remote_history": stored}
    FIXTURE_STATE.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_STATE.write_bytes(json.dumps(state).encode("utf-8"))
    _stop_app()
    CONFIG.write_bytes((json.dumps(capture_config(json.loads(base64.b64decode(state["config"])), fx), indent=2) + "\n").encode("utf-8"))
    _start_app(exe)
    replay(fx, FIXTURE_SCRATCH)
    check_shown(fx, wait_s=8)
    print(f"fixture: {len(fx['rows'])} rows shown from {Path(path).name}")


def fixture_down() -> None:
    """Put the dashboard back exactly as `fixture_up` found it.

    Stopping the app drops the fixture's rows, so nothing is ended through the API.
    What the rows left behind is undone in the stores while the app is down, where
    none of its own writes can race the edit: the history entries of their row
    ids, the session ids they mapped to rows, and a pushed device's stored history.
    """
    if not FIXTURE_STATE.exists():
        raise CaptureError(f"No fixture run to put back: {FIXTURE_STATE} does not exist.")
    state = json.loads(FIXTURE_STATE.read_text(encoding="utf-8"))
    fx = load_fixture(Path(state["fixture"]))
    _stop_app()
    CONFIG.write_bytes(base64.b64decode(state["config"]))

    def restore_history(data: dict) -> None:
        for project, before in state["history"].items():
            if before is None:
                data.pop(project, None)
            else:
                data[project] = before

    def forget_sessions(data: dict) -> None:
        for row in fx["rows"]:
            data.pop(_session_id(row), None)

    _edit_json(HISTORY, restore_history)
    _edit_json(SESSION_IDS, forget_sessions)
    for device, before in state["remote_history"].items():
        f = REMOTE_HISTORY / f"{device}.json"
        if before is None:
            f.unlink(missing_ok=True)
        else:
            f.write_bytes(base64.b64decode(before))
    _start_app(state["exe"])
    shutil.rmtree(FIXTURE_SCRATCH, ignore_errors=True)
    FIXTURE_STATE.unlink()
    print("fixture: dashboard restored")


# The BLOCK pill's background (`.state-blocked` in SessionItem.svelte). Its
# `pulse` animation runs its opacity between 1 and 0.45 every 1.6s, so a frame
# catches it wherever the shutter lands.
BLOCK_PILL = (0xB4, 0x53, 0x09)


def pill_opacities(png: Path, pill: tuple[int, int, int] = BLOCK_PILL) -> list[float]:
    """The opacity each pill of one colour was photographed at, top to bottom.

    A pixel of a pill at opacity `o` over the row background `bg` reads
    `bg + (pill - bg) * o` in every channel, so each channel gives its own
    estimate of `o`. Pixels whose three estimates agree on an `o` above 0.25 are
    pill. All three are needed: ClearType's coloured text fringes match the pill's
    red and green but not its blue, and the pill's own text differs in hue
    altogether. Pills are told apart by the gaps between their pixel rows; a
    cluster too small to be a pill is dropped, and each pill reports its median.
    """
    im = Image.open(png).convert("RGBA")
    w, h = im.size
    px = im.load()
    counts: dict[tuple[int, int, int], int] = {}
    for y in range(0, h, 4):
        for x in range(0, w, 4):
            r, g, b, a = px[x, y]
            if a == 255:
                counts[(r, g, b)] = counts.get((r, g, b), 0) + 1
    bg = max(counts, key=counts.get)
    by_row: dict[int, list[float]] = {}
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            if a < 200:
                continue
            o = [(c - base) / (target - base) for c, base, target in zip((r, g, b), bg, pill)]
            if o[0] > 0.25 and max(o) - min(o) < 0.15:
                by_row.setdefault(y, []).append(o[0])
    pills: list[list[float]] = []
    last = -10
    for y in sorted(by_row):
        if y - last > 2:
            pills.append([])
        pills[-1].extend(by_row[y])
        last = y
    return [sorted(v)[len(v) // 2] for v in pills if len(v) > 150]


# ------------------------------------------------------------- what may be seen

# Projects whose sessions may appear in a committed frame.
#
# A row publishes its project name, and outside compact mode it publishes the
# session's real prompt as well, so a frame carrying a project that should not be
# published puts both on the README and the docs site. The rule applies to every
# figure; the list lives here because four scripts enforce it and a copy per
# script is four lists to forget to update.
#
# THE NAME IS "PUBLISHABLE" AND NOT "PUBLIC", because the two are not the same
# question and the difference is one entry. Five of these were read off GitHub
# rather than assumed, and are public repositories: agterm (umputun/agterm),
# bga-assistant (AnotherSava/bga-assistant), claude
# (AnotherSava/claude-code-common), tauri-dashboard
# (AnotherSava/claude-code-dashboard) and achievement-overlay
# (AnotherSava/achievement-overlay), that last one reaching a frame from the other
# machine rather than this one. The sixth, what-is-next
# (AnotherSava/what-is-next), is PRIVATE, and is here because its owner cleared it
# for these frames on 2026-09-11 knowing that the frames carry its prompt text and
# not only its name. Only the owner can give that clearance, so a future entry
# needs the same thing rather than an inference from the repository's visibility —
# and an entry that is here on a clearance rather than on a reading is the one to
# re-check first, since the clearance was for the task that session was on then.
#
# RENAMES BELONG HERE TOO, which is why this is a list of names and not of
# repositories. A row draws `display_name` when the user has given it one, so the
# rename is the string in the picture and the check has to cover it: `ai-dashboard`
# is what both machines call their `tauri-dashboard` row, and without it the guard
# refuses a frame of an entirely public project. A rename is also the one entry
# that can appear without any repository behind it at all.
#
# It is a DEFAULT and not a policy: a checkout under a different directory name
# derives a different row id, so every caller takes a `--publishable` override.
PUBLISHABLE_PROJECTS = ("achievement-overlay", "agterm", "ai-dashboard", "bga-assistant", "claude", "tauri-dashboard", "what-is-next")


def names_in_frame(row: dict) -> list[str]:
    """Every project name one roster row puts on screen.

    The widget draws `display_name` when there is one and the de-namespaced id
    otherwise — the roster's `project`, equal to `id` for a local row — with the
    device in its own pill. A renamed row is checked on both: the rename is the
    string in the picture, and the project is the thing that has to be public.

    Here rather than in a script because two figures publish row names and both
    need the same answer; a copy each is two definitions to keep in step with
    whatever `SessionItem.svelte` draws.
    """
    names = [row.get("project") or row.get("id", "")]
    if row.get("display_name"):
        names.append(row["display_name"])
    return names


def assert_publishable(names, allowed=PUBLISHABLE_PROJECTS, where: str = "the frame") -> None:
    """Refuse when anything about to be photographed names a project not known to be public."""
    unknown = sorted({n for n in names if n and n.split("/")[-1] not in allowed})
    if unknown:
        raise CaptureError(
            f"{', '.join(repr(u) for u in unknown)} would be in {where}, and {'it is' if len(unknown) == 1 else 'they are'} not on the public list "
            f"({', '.join(allowed)}). A frame publishes the project name, and outside compact mode the session's prompt too. "
            f"End those sessions before capturing, or pass --publishable NAME ... if the project really is public."
        )


# --- Command-line entry point, for the PowerShell capture scripts -------------
#
# The Windows half of this project is PowerShell and cannot import a Python
# module, so without this the publishable-names rule would have to be written
# twice — and the list's own comment above says why that is the thing to avoid:
# a copy per caller is a copy to forget to update. The Windows frame step already
# shells out to the skill's `winframe.py`, so the shape is proven on that machine.
#
# WHAT CROSSES THE BOUNDARY IS THE ROSTER, NOT THE NAMES, deliberately. Handing
# over a list of names would put `names_in_frame` — the rule about what a row
# actually draws — on the PowerShell side, which is the half most likely to
# drift: it encodes what `SessionItem.svelte` renders, and a renamed row is
# exactly the case a hand-rolled extraction gets wrong. So the caller pipes the
# `/api/agents` response in verbatim and this decides both what is on screen and
# whether it may be published.
#
#     ... | python lib/dashboard.py assert-publishable --where "the frame" [--name X] [--publishable A B]
#
# Exit 0 to say nothing would be published that should not be; exit 1 with the
# refusal on stderr. `--name` adds strings the roster does not carry — a window
# title, a tab caption — which are checked against the same list.
def _cli(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="dashboard.py", description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    ck = sub.add_parser("assert-publishable", help="refuse if the roster on stdin would put a non-publishable project on screen")
    ck.add_argument("--where", default="the frame", help="what is being photographed, for the refusal message")
    ck.add_argument("--name", action="append", default=[], metavar="NAME", help="an extra on-screen string the roster does not carry; repeatable")
    ck.add_argument("--publishable", nargs="+", metavar="NAME", default=list(PUBLISHABLE_PROJECTS), help="project names allowed on screen; replaces the built-in list rather than extending it")
    ck.add_argument("--local-only", action="store_true", help="check only rows the roster marks local (a frame taken with sync off)")
    for name, help_text in (("fixture-up", "restart the dashboard on a capture config and replay a fixture's rows"), ("fixture-check", "refuse unless the dashboard shows exactly a fixture's rows")):
        sub.add_parser(name, help=help_text).add_argument("fixture", type=Path)
    sub.add_parser("fixture-down", help="restore the dashboard a fixture-up changed")
    pc = sub.add_parser("pulse-check", help="refuse a frame whose BLOCK pills were caught dimmer than --min")
    pc.add_argument("png", type=Path)
    pc.add_argument("--min", type=float, default=0.9, help="lowest acceptable pill opacity")
    args = ap.parse_args(argv)

    if args.cmd == "pulse-check":
        found = pill_opacities(args.png)
        print(f"BLOCK pill opacity: {', '.join(f'{o:.2f}' for o in found) or 'none found'}")
        return 0 if found and min(found) >= args.min else 1

    if args.cmd.startswith("fixture-"):
        try:
            if args.cmd == "fixture-up":
                fixture_up(args.fixture)
            elif args.cmd == "fixture-check":
                check_shown(load_fixture(args.fixture))
            else:
                fixture_down()
        except CaptureError as e:
            print(f"dashboard.py: {e}", file=sys.stderr)
            return 1
        return 0

    blob = sys.stdin.read().strip()
    if not blob:
        print("dashboard.py: nothing on stdin; pipe the /api/agents response in.", file=sys.stderr)
        return 2
    rows = json.loads(blob).get("agents", [])
    if args.local_only:
        rows = [r for r in rows if r.get("local")]
    names = [n for r in rows for n in names_in_frame(r)] + list(args.name)
    allowed = tuple(sorted({n.strip() for n in args.publishable if n.strip()}))
    try:
        assert_publishable(names, allowed=allowed, where=args.where)
    except CaptureError as e:
        print(f"dashboard.py: {e}", file=sys.stderr)
        return 1
    print(f"publishable: {len(names)} name(s) checked in {args.where}, all on the list")
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli(sys.argv[1:]))
