#!/usr/bin/env python3
"""The tray icon in each usage-badge style, for the Usage limits section of the
features page: `tray-badges-windows.png` and `tray-badges-macos.png`.

Needs no running app and takes nothing on screen. The icons are the product's
own pixels: `tray_badge::render`, the dispatch `refresh` sets the tray icon
from, called by the ignored test `render_the_docs_figure` against the working
tree. This script hands that test each platform's base icon and display scale
and lays the results out in a strip. The tray size comes from the test, through
`tray_icon_px`, the rule the tray itself is sized by, and the strip's grouping
from the group each icon's file name carries.

What each platform starts from is what Tauri hands `refresh` as
`default_window_icon`, read here off `tauri.conf.json`'s bundle icon list. On
Windows that is the `.ico` entry's frame at the system's large icon size, 32 px
at 96 DPI times the scale. On macOS it is the list's first PNG. Those two rules
are Tauri's and are restated here, so a Tauri upgrade that changes them leaves
this figure drawn from the wrong base.

The strip, left to right: the lights style at 30%, 60% and 90%; the number
style at 42%, 87% and 100%, where it switches to the all-red light; then the
context alert on each style, the lights at 60% and the number at 42%. The
icons sit at their native size on a flat stand-in for the tray, Windows 11's
dark taskbar and the macOS light menu bar, and `hairline.py` gives the strip its
edge.

    python docs/screenshots/capture/tray-badges.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
import dashboard as dash  # noqa: E402

SRC_TAURI = dash.REPO / "src-tauri"
SCALE = 2.0   # the display scale the figure is drawn for: 200% on Windows, Retina on macOS

# (platform, tray background)
PLATFORMS = [("windows", (32, 32, 32)), ("macos", (246, 246, 246))]


def base_icon(platform: str) -> Image.Image:
    """What Tauri hands `refresh` as `default_window_icon` on `platform`."""
    icons = json.loads((SRC_TAURI / "tauri.conf.json").read_text(encoding="utf-8"))["bundle"]["icon"]
    if platform == "windows":
        frame = round(32 * SCALE)
        return Image.open(SRC_TAURI / next(i for i in icons if i.endswith(".ico"))).ico.getimage((frame, frame))
    return Image.open(SRC_TAURI / next(i for i in icons if i.endswith(".png")))


def render(platform: str, out: Path) -> list[tuple[str, Image.Image]]:
    """The tray bitmaps for `platform`, each with its strip group, from the Rust test."""
    base = base_icon(platform).convert("RGBA")
    raw = out / "base.rgba"
    raw.write_bytes(base.tobytes())
    env = dict(os.environ, TRAY_FIGURE_BASE=str(raw), TRAY_FIGURE_BASE_W=str(base.width), TRAY_FIGURE_BASE_H=str(base.height), TRAY_FIGURE_PLATFORM=platform, TRAY_FIGURE_SCALE=str(SCALE), TRAY_FIGURE_OUT=str(out))
    cmd = ["cargo", "test", "--manifest-path", str(SRC_TAURI / "Cargo.toml"), "--lib", "tray_badge::tests::render_the_docs_figure", "--", "--ignored", "--exact"]
    r = subprocess.run(cmd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if r.returncode != 0:
        sys.exit(f"the render test failed:\n{r.stdout[-3000:]}\n{r.stderr[-3000:]}")
    files = sorted(out.glob("[0-9][0-9]-*.rgba"))
    if not files:
        sys.exit("the render test wrote no icons")
    icons = []
    for f in files:
        data = f.read_bytes()
        size = round((len(data) // 4) ** 0.5)
        if size * size * 4 != len(data):
            sys.exit(f"{f.name} is not a square RGBA bitmap")
        icons.append((f.stem.split("-")[1], Image.frombytes("RGBA", (size, size), data)))
    return icons


def strip(icons: list[tuple[str, Image.Image]], background: tuple[int, int, int]) -> Image.Image:
    """The icons in a row, a half-icon apart inside a group and a full icon and
    a half between groups, with a half-icon margin."""
    size = icons[0][1].width
    gap, group_gap, margin = size // 2, size + size // 2, size // 2
    xs, x = [], margin
    for i, (group, _) in enumerate(icons):
        if i:
            x += size + (gap if group == icons[i - 1][0] else group_gap)
        xs.append(x)
    canvas = Image.new("RGBA", (xs[-1] + size + margin, size + 2 * margin), background + (255,))
    for (_, icon), at in zip(icons, xs):
        canvas.alpha_composite(icon, (at, margin))
    return canvas.convert("RGB")


def main() -> None:
    hairline = dash.skill_script("hairline.py")
    for platform, background in PLATFORMS:
        with tempfile.TemporaryDirectory() as tmp:
            icons = render(platform, Path(tmp))
        path = dash.SHOTS / f"tray-badges-{platform}.png"
        strip(icons, background).save(path)
        raw = dash.SHOTS / "raw" / path.name
        raw.parent.mkdir(exist_ok=True)
        shutil.copy(path, raw)
        r = subprocess.run([sys.executable, str(hairline), "--require", str(path)], capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"hairline.py failed on {path.name}: {r.stderr.strip()}")
        print(f"wrote {path.relative_to(dash.REPO)}")


if __name__ == "__main__":
    main()
