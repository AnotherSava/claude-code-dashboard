#!/usr/bin/env python3
"""Reconcile the four places a documentation screenshot is named: the pages that
include it, the PNGs on disk, the entries in screenshots.json, and README.md.

It also validates the one manifest field whose value nothing else reads --
`verifiedAt` -- because a wrong value there is otherwise invisible.

Errors are contradictions and fail the build. Two things are reported and never
failed, because both are legitimate resting states of this repo and a check that
goes red on a known state is one somebody disables within a week:

  * a figure whose other platform has not been captured yet
  * a frame that cannot be verified from this machine

Both print even when the count is zero, so silence never reads as "nothing to do".
"""
import json
import re
import subprocess
import sys
from pathlib import Path

import yaml
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / "docs"
SHOTS = DOCS / "screenshots"

# Read off the same file the site reads, never a second copy of the list. The
# suffix in `<figure>-<platform>.png`, the manifest id suffix and `<html
# data-os>` are all one value, and `figure.html` derives every one of them from
# `_data/platforms.yml`; a literal here would be a gate that passes a third
# platform the site renders and fails one it does not.
PLATFORMS = tuple(p["key"] for p in yaml.safe_load((DOCS / "_data" / "platforms.yml").read_text(encoding="utf-8")))

INCLUDE = re.compile(r"\{%-?\s*include\s+figure\.html\s+(.*?)-?%\}", re.S)
RAW_IMG = re.compile(
    # Non-greedy across the alt text: `![A tab showing [67%]](...)` is a legal
    # image whose alt contains a bracket, and a `[^\]]*` alt misses it entirely.
    r"!\[.*?\]\([^)]*screenshots/[^)]+\)"
    r"|<img[^>]*screenshots/[^>]*>",
    re.S,
)
README_IMG = re.compile(r"docs/screenshots/([A-Za-z0-9._-]+\.png)")
# The README's hand-written stand-in for the note figure.html derives; see main().
UNTAKEN = "hasn't been taken yet"


# The sentinel `verifiedAt` carries when a shot was examined as part of the very
# commit that introduces it -- see the note in `main`. Resolve it with
# `git log -1 --format=%h -- docs/screenshots/<id>.png`.
AT_CAPTURE = "at-capture"


def commit_exists(sha: str) -> bool:
    """Whether `sha` names a commit here.

    Answers True for anything it cannot rule out, which is the direction that
    matters: this runs in CI and in `commit-checks.sh`, so a checkout without git
    -- a source tarball, a vendored copy -- must not fail the build over a field
    nothing reads at runtime. Only a live repo that positively denies the object
    is an error. The two cases are separated deliberately: folding them together
    is how the first version of this returned True for a sha that did not exist.
    """
    try:
        in_repo = subprocess.run(["git", "rev-parse", "--git-dir"], cwd=SHOTS, capture_output=True, timeout=10)
        if in_repo.returncode != 0:
            return True                    # not a repository: nothing to check against
        r = subprocess.run(["git", "cat-file", "-t", sha], cwd=SHOTS, capture_output=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return True                        # no git, or it hung: not this check's business
    return r.returncode == 0 and r.stdout.strip() == b"commit"


def arg(name: str, blob: str) -> str | None:
    m = re.search(r'\b%s="([^"]*)"' % name, blob)
    return m.group(1) if m else None


# The macOS window-chrome traffic lights, by hue band rather than by RGB value.
# The documented Apple colours do not survive a real capture: measured on this
# machine's frames the lights read (253,45,53), (250,199,0) and (0,186,40)
# against the nominal ff5f57 / febc2e / 28c840, and an RGB-with-tolerance test
# tight enough to be meaningful missed all three. A hue band plus a saturation
# floor is what actually identifies "a saturated red/amber/green dot".
# Red straddles 0deg -- the measured light is (253,45,53), which is hue 358, not
# hue 2 -- so hues above 330 are read as negative and the red band spans zero.
HUE_BANDS = ((-14, 14), (40, 60), (105, 150))   # red, amber, green, in degrees
SAT_MIN, VAL_MIN = 0.55, 0.45
BAND_H, BAND_W = 80, 400
MIN_RUN = 4                  # a dot, not a stray antialiased pixel
MIN_ROWS = 3                 # a dot is round, so the row repeats down its height
GAP_MIN, GAP_MAX = 18, 60    # measured 46 at 2x on this machine's frames; 1x halves it
GAP_SKEW = 0.35              # the three are evenly spaced, so the two gaps match


def _hsv(r: int, g: int, b: int) -> tuple[float, float, float]:
    import colorsys
    h, s, v = colorsys.rgb_to_hsv(r / 255, g / 255, b / 255)
    deg = h * 360
    return (deg - 360 if deg > 330 else deg), s, v


def _band(px, x: int, y: int) -> int | None:
    """Which traffic-light hue this pixel is, if it is saturated enough to be one."""
    hue, sat, val = _hsv(*px[x, y])
    if sat < SAT_MIN or val < VAL_MIN:
        return None
    for i, (lo, hi) in enumerate(HUE_BANDS):
        if lo <= hue <= hi:
            return i
    return None


def has_mac_chrome(path: Path) -> bool:
    """A red, amber and green dot side by side on one scanline, near the top-left.

    The geometry is what makes this a test rather than a hue count, and it has
    been wrong in both directions. The audit that started this work called
    compact.png a Windows capture because a bare hue probe matched green alone --
    which was that frame's DONE pill, on a macOS shot. The first fix for that
    took the first hit per hue anywhere in an 80x400 box and allowed the three to
    sit 8 rows apart and up to 130px apart, which any row of coloured status
    glyphs satisfies: terminal-tabs-windows.png puts a raised hand at y=32 and a
    green circle at y=29, and a Windows frame wrongly failing the gate is the
    expensive direction, since this runs in CI and in commit-checks.sh.

    So the three must be RUNS on ONE scanline, in order, evenly spaced, and the
    row must repeat -- a dot is round, so a real light spans many scanlines while
    a coincidence of three glyph pixels spans one. Measured over the committed
    set: 28 qualifying rows on each decorated macOS frame, zero on every Windows
    frame and zero on the undecorated macOS widget shots.
    """
    im = Image.open(path).convert("RGB")
    w, h = im.size
    px = im.load()
    rows = 0
    for y in range(min(BAND_H, h)):
        runs: list[tuple[int, float]] = []       # (hue band, centre x)
        x = 0
        limit = min(BAND_W, w)
        while x < limit:
            b = _band(px, x, y)
            if b is None:
                x += 1
                continue
            x0 = x
            while x < limit and _band(px, x, y) == b:
                x += 1
            if x - x0 >= MIN_RUN:
                runs.append((b, x0 + (x - x0) / 2))
        for i in range(len(runs) - 2):
            (ba, ca), (bb, cb), (bc, cc) = runs[i:i + 3]
            if (ba, bb, bc) != (0, 1, 2):
                continue
            g1, g2 = cb - ca, cc - cb
            if not all(GAP_MIN <= g <= GAP_MAX for g in (g1, g2)):
                continue
            if abs(g1 - g2) > GAP_SKEW * max(g1, g2):
                continue
            rows += 1
            break
    return rows >= MIN_ROWS


def main() -> int:
    errors: list[str] = []
    gaps: list[str] = []
    unverifiable: list[str] = []

    manifest = json.loads((SHOTS / "screenshots.json").read_text(encoding="utf-8"))
    entries = {e["id"]: e for e in manifest["screenshots"]}
    if len(entries) != len(manifest["screenshots"]):
        errors.append("screenshots.json: two entries share an id")

    # `verifiedAt` is either a commit sha or the sentinel `at-capture`; absent
    # means never examined. The skill's references/screenshot-manifest.md defines
    # all three and why the sentinel exists — in short, the sha of the commit that
    # CARRIES a shot cannot be written while composing that commit, so recording
    # one would otherwise cost a second commit per change.
    #
    # Checked here because nothing reads the value, so a wrong one is invisible:
    # a date was written into this field on 2026-09-13 and nothing noticed.
    for eid, e in entries.items():
        v = e.get("verifiedAt")
        if v is None or v == AT_CAPTURE:
            continue
        if not re.fullmatch(r"[0-9a-f]{7,40}", v):
            errors.append(f"{eid}: verifiedAt is {v!r}, which is neither a commit sha nor {AT_CAPTURE!r}")
        elif not commit_exists(v):
            errors.append(f"{eid}: verifiedAt {v!r} is not a commit in this repository")

    on_disk = {p.name for p in SHOTS.glob("*.png")}
    claimed: dict[str, str] = {}

    for eid, e in entries.items():
        for f in e["files"]:
            if f in claimed:
                errors.append(f"screenshots.json: {f} is claimed by both {claimed[f]} and {eid}")
            claimed[f] = eid
            if f not in on_disk:
                errors.append(f"screenshots.json: {eid} lists {f}, which is not on disk — orphaned metadata")
        if e["files"] != [f"{eid}.png"]:
            errors.append(f"screenshots.json: {eid} must carry exactly one file, named {eid}.png")
        if not any(eid.endswith(f"-{p}") for p in PLATFORMS):
            errors.append(f"screenshots.json: {eid} does not end with -windows or -macos")

    for name in sorted(on_disk):
        stem = name[:-4]
        if not any(stem.endswith(f"-{p}") for p in PLATFORMS):
            errors.append(f"{name}: not named <figure>-windows.png or <figure>-macos.png")
        if stem not in entries:
            errors.append(f"{name}: no entry in screenshots.json (id should be {stem!r})")

    # Does the picture agree with the platform its name claims?
    #
    # This test has exactly one direction. Finding macOS window chrome DISPROVES a
    # `-windows` name; finding none proves nothing at all, because an undecorated
    # window has no title bar to read on either platform. The dashboard's own
    # widget is `"decorations": false`, so `hero` and `compact-mode` are
    # chrome-free on BOTH platforms -- and the defect that started this work was
    # precisely one of those: `compact.png`, a macOS widget shot filed as Windows.
    #
    # So a silent pass here would be the reassuring kind of nothing. Every frame
    # the test cannot rule on is listed instead, whichever platform it claims.
    for name in sorted(on_disk):
        stem = name[:-4]
        mac = has_mac_chrome(SHOTS / name)
        if stem.endswith("-windows") and mac:
            errors.append(f"{name} carries macOS window chrome but is filed as a Windows capture")
        elif not mac:
            unverifiable.append(
                f"{name} — no window chrome to read, so its platform cannot be confirmed "
                f"(an undecorated window looks the same on both)"
            )

    calls: dict[str, dict] = {}
    for md in DOCS.rglob("*.md"):
        rel = md.relative_to(DOCS).parts
        # Relative to DOCS, never the absolute path: a clone under ~/plans or a
        # CI checkout beneath a "vendor" directory would otherwise skip every
        # page, leaving `calls` empty and the run misleadingly quiet.
        if {"plans", "vendor", "_site"} & set(rel):
            continue
        where = md.relative_to(ROOT).as_posix()
        text = md.read_text(encoding="utf-8")
        for m in INCLUDE.finditer(text):
            blob = m.group(1)
            fid, only = arg("id", blob), arg("only", blob)
            if only is not None and not only.strip():
                # Liquid treats "" as truthy and Python does not, so an empty
                # only= is the one value the two languages score differently.
                errors.append(f'{where}: figure {fid} has only="" — omit the argument instead')
                only = None
            if not fid:
                errors.append(f"{where}: a figure.html include has no id")
                continue
            if fid in calls:
                errors.append(f"figure {fid!r} is included by both {calls[fid]['where']} and {where}")
            if only and only not in PLATFORMS:
                errors.append(f'{where}: figure {fid} has only="{only}", expected windows or macos')
            if not (arg("alt", blob) or (arg("alt_windows", blob) and arg("alt_macos", blob))):
                errors.append(f"{where}: figure {fid} has no alt text (need alt, or both alt_windows and alt_macos)")
            calls[fid] = {"only": only, "where": where}
        for m in RAW_IMG.finditer(text):
            errors.append(
                f"{where}: {m.group(0)[:60]} — screenshots go through "
                "{% include figure.html %}, which names the platform"
            )

    for fid, call in sorted(calls.items()):
        present = [p for p in PLATFORMS if f"{fid}-{p}.png" in on_disk]
        if not present:
            errors.append(f"{call['where']}: figure {fid} has no variant on disk")
        if call["only"]:
            for p in present:
                if p != call["only"]:
                    errors.append(f'{call["where"]}: figure {fid} is only="{call["only"]}" but {fid}-{p}.png exists')
        else:
            for p in PLATFORMS:
                if p not in present:
                    gaps.append(f"{fid} — no {p} variant ({call['where']})")

    for name in sorted(on_disk):
        stem = name[:-4].rsplit("-", 1)[0]
        if stem not in calls:
            errors.append(f"{name}: on disk and in the manifest, included by no page")

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    readme_figures: set[str] = set()
    for f in README_IMG.findall(readme):
        if f not in on_disk:
            errors.append(f"README.md links docs/screenshots/{f}, which is not on disk")
        elif f not in claimed:
            errors.append(f"README.md links docs/screenshots/{f}, which has no entry in screenshots.json")
        elif not any(f[:-4].endswith(f"-{p}") for p in PLATFORMS):
            errors.append(f"README.md links {f}, whose name states no platform")
        else:
            readme_figures.add(f[:-4].rsplit("-", 1)[0])

    # The README says in prose what figure.html reads off disk.
    #
    # Inside the docs site a missing variant needs no prose at all: the include
    # probes site.static_files and prints the note itself, so the day the frame
    # is captured the note stops appearing with nothing to remember. The README
    # is rendered by GitHub with no Liquid, so the same sentence has to be typed
    # -- and a typed one goes stale silently, which is the whole reason this
    # file exists. So it is asserted instead: the sentence must be present
    # exactly while a variant of a figure the README embeds is genuinely absent.
    # A figure declared `only=` for one platform has no missing variant, it has
    # one that cannot exist -- the same distinction the docs-side gap loop draws
    # above, and the reason `only` exists at all. Reading it here too is not a
    # nicety: without it a legitimate `only="macos"` figure in the README fails
    # the build, and the ONLY way to satisfy the error is to write a sentence
    # saying its Windows frame "hasn't been taken yet", which is false. This gate
    # runs in CI and in commit-checks.sh, so that is a blocked commit whose fix
    # is a lie.
    readme_missing = sorted(
        f"{fig}-{p}.png"
        for fig in readme_figures
        for p in PLATFORMS
        if f"{fig}-{p}.png" not in on_disk
        and not (calls.get(fig, {}).get("only") and calls[fig]["only"] != p)
    )
    says_untaken = UNTAKEN in readme
    if readme_missing and not says_untaken:
        errors.append(
            f"README.md embeds a figure whose other variant is missing ({', '.join(readme_missing)}) "
            f"but says nothing about it — add a line containing {UNTAKEN!r}, or capture the frame"
        )
    elif says_untaken and not readme_missing:
        errors.append(
            f"README.md still says {UNTAKEN!r}, but every variant of every figure it embeds "
            f"is on disk — drop the sentence and show both"
        )

    print(f"{len(calls)} figures, {len(on_disk)} variants on disk, {len(entries)} manifest entries")
    print(f"figures missing a variant: {len(gaps)}")
    for g in gaps:
        print(f"  NOT COVERED  {g}")
    print(f"variants whose platform the pixels cannot confirm: {len(unverifiable)}")
    for u in unverifiable:
        print(f"  NOT COVERED  {u}")
    for e in errors:
        print(f"  ERROR  {e}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
