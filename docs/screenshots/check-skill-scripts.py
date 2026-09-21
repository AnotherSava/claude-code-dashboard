#!/usr/bin/env python3
"""Every shared capture script the figures depend on still resolves on this machine.

The capture libs do not carry their own screenshot machinery. Taking the picture,
listing windows and stroking an edge belong to the docs-relevance skill in the
dotfiles, so a fix reaches every project at once — and the price of that split is
a path this repo hardcodes and the dotfiles can rename out from under it.

They did, on 2026-09-17: the skill was called `documentation` until then, and both
libs went on resolving the old path. Nothing noticed for three days, because
nothing runs a capture except a person re-shooting a figure, and the failure only
surfaces at the shutter — `hairline.py is missing`, after the window has been
staged. `check-figures.py` cannot catch it either: it reads the committed frames
and the manifest, never the libs that produce them.

WHY THIS IS NOT IN `docs.yml`, unlike its neighbour. The paths resolve under the
developer's `~/.claude`, which no CI runner has, so asserting them there would
fail every run. That makes this the second deliberate exception to the lockstep
rule in `.claude/commit-checks.sh` — the conventions checker is the first, and for
the same reason: both measure the machine a capture would actually run on. Adding
this to a workflow would not strengthen the gate, it would break it.

The reference list is read out of the source rather than restated here, so a new
`skill_script("...")` call is covered the day it is written and this file cannot
drift into asserting a set the libs no longer use.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CAPTURE = ROOT / "docs" / "screenshots" / "capture"

sys.path.insert(0, str(CAPTURE / "lib"))
import dashboard  # noqa: E402  the lib owns SKILL_SCRIPTS; re-deriving it here would be the second copy this file exists to prevent


def _referenced() -> list[tuple[str, Path]]:
    """Every shared script the capture tree names, as (where it is named, where it must be)."""
    found: list[tuple[str, Path]] = []
    for src in sorted(CAPTURE.rglob("*.py")):
        rel = src.relative_to(ROOT)
        for name in re.findall(r'skill_script\(\s*"([^"]+)"', src.read_text(encoding="utf-8")):
            found.append((f"{rel}: skill_script({name!r})", dashboard.SKILL_SCRIPTS / name))
    # The PowerShell half builds its path inline rather than through a helper, so it is
    # matched on the `$env:USERPROFILE '...'` literal. Read as utf-8-sig: these files carry
    # a BOM, and a stray ﻿ would silently break the first match.
    for src in sorted(CAPTURE.rglob("*.ps1")):
        rel = src.relative_to(ROOT)
        for lit in re.findall(r"\$env:USERPROFILE\s+'([^']+)'", src.read_text(encoding="utf-8-sig")):
            found.append((f"{rel}: {lit}", Path.home().joinpath(*lit.split("\\"))))
    return found


def main() -> None:
    wanted = _referenced()
    # An empty result is a failure, not a pass. Both patterns above match a shape the libs
    # chose and could rewrite; silently checking nothing would read exactly like checking
    # everything and finding it well.
    if not wanted:
        sys.exit("check-skill-scripts: matched no shared-script references at all — the patterns here have gone stale, not the paths.")
    missing = [(where, p) for where, p in wanted if not p.exists()]
    for where, p in missing:
        print(f"MISSING  {p}\n         referenced by {where}", file=sys.stderr)
    if missing:
        sys.exit(f"check-skill-scripts: {len(missing)} of {len(wanted)} shared capture scripts do not resolve. The dotfiles skill holding them was probably renamed; update the libs to match, then re-run.")
    print(f"check-skill-scripts: {len(wanted)} shared capture script references resolve.")


if __name__ == "__main__":
    main()
