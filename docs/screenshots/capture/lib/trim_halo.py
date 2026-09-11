#!/usr/bin/env python3
"""Clear the compositor's drop shadow from the corners of a captured window.

Both platforms leave a halo outside the window's rounded edge, and neither is
part of the window:

  Windows  a flat shadow filling the whole corner square at alpha 14-40 over
           black. On a light documentation page that renders as a pale grey
           square sitting outside the round -- "the corners are not transparent".
  macOS    a single ring one pixel outside the edge at alpha 9, dark on the
           widget and blue on the agterm frame, which reads as a dirty outline.

A pixel is halo when it is faint OR it is black, and neither test alone covers
both platforms. Measured on the committed set:

  top corner, Windows   alpha 14,15,18 then 64,114,255 -- shadow is faint
  bottom corner, Windows alpha 92,95,99 then 131,165,255 -- shadow is NOT faint,
           but it is pure (0,0,0) while the edge below it is (43,43,43),
           (73,73,73), (28,28,30). The solve returns black for shadow because a
           shadow only darkens the backdrop, so colour is the discriminator here.
  macOS    a single alpha-9 ring, coloured (28,28,28) on the widget and
           (57,142,198) on the agterm frame -- not black at all, but very faint.

So: faint (alpha <= 56) or black (max channel <= 16), and never a fully opaque
pixel. Every genuine edge pixel measured fails both tests.

Two things keep this from eating anything real:

  * It is a FLOOD FILL FROM THE CORNER, not a threshold over the image. Only
    low-alpha pixels reachable from the corner are cleared, so a translucent
    pixel inside the window is never touched however faint it is.
  * It is BOUNDED to a corner box. The halo runs 3px (Windows) to 6px (macOS)
    down the diagonal; the box is 48px, which is a wide margin and still cannot
    reach the middle of an edge -- where Windows' real border measures alpha 143
    and would otherwise be a candidate for any global rule.

Usage:  trim_halo.py <png> [<png> ...]
"""
import sys
from collections import deque
from pathlib import Path

from PIL import Image

FAINT = 56      # alpha at or below this is halo whatever its colour
BLACK = 16      # any channel above this is a real edge pixel, however solid
BOX = 48


def is_halo(p) -> bool:
    r, g, b, a = p
    if a >= 255:
        return False
    return a <= FAINT or max(r, g, b) <= BLACK


def trim(path: Path) -> int:
    im = Image.open(path).convert("RGBA")
    w, h = im.size
    px = im.load()
    box = min(BOX, w // 2, h // 2)
    cleared = 0

    for cx, cy in ((0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)):
        x0, x1 = (0, box) if cx == 0 else (w - box, w)
        y0, y1 = (0, box) if cy == 0 else (h - box, h)
        if not is_halo(px[cx, cy]):
            continue                      # no halo in this corner; nothing to do
        seen = {(cx, cy)}
        queue = deque([(cx, cy)])
        while queue:
            x, y = queue.popleft()
            px[x, y] = (0, 0, 0, 0)
            cleared += 1
            for nx, ny in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
                if not (x0 <= nx < x1 and y0 <= ny < y1):
                    continue
                if (nx, ny) in seen or not is_halo(px[nx, ny]):
                    continue
                seen.add((nx, ny))
                queue.append((nx, ny))

    if cleared:
        im.save(path)
    return cleared


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    for arg in sys.argv[1:]:
        p = Path(arg)
        n = trim(p)
        print(f"{p.name}: {n} halo pixels cleared")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
