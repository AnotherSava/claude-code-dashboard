<#
  Windows variant of the `terminal-tabs` figure.

  The macOS half of this figure is agterm's session sidebar. There is no such
  sidebar on Windows: the same information rides Windows Terminal's tab strip,
  written into each session's console title. So this is not a re-shoot of the
  macOS frame on another machine, it is a different picture of the same feature,
  and the two carry per-variant captions in the docs saying which application
  each one is.

  A tab strip is a band across the top of a window and cannot be asked for
  directly, so the whole window is captured and cropped twice, both bounds
  measured rather than hardcoded:

    height  the strip is a flat colour and the terminal underneath it is not, so
            the first row that differs from the strip's own background ends it.
            Measuring beats a constant because the strip is 32 logical px and a
            hardcoded 48 would take a third of the terminal with it on a 1x
            display.
    width   the last column holding anything other than strip background, plus a
            little padding, so the frame ends just past the new-tab button
            instead of carrying a metre of empty bar and the window controls.

  -Method Alpha, like the other Windows frames: the strip keeps the window's own
  rounded top corners and its border, with everything outside them transparent.
  It reads pixels off the screen, so the window has to be unobscured — which is
  also why the widget is hidden for the shot. PrintWindow is not an option here
  at all: it returns a stale or blank surface for Windows Terminal's XAML
  islands.
#>
[CmdletBinding()]
param(
    # Which Windows Terminal window to photograph, matched on its title — which
    # is its active tab's title, which is a title this dashboard wrote. Needed
    # whenever more than one terminal window is open, and the frame is normally
    # staged in a window of its own so it carries only the sessions it is about.
    [string]$TitleLike,
    # An exact window handle, for when the frame is staged in a terminal window
    # of its own: the staging knows which window it just created, and a title
    # match cannot tell two windows apart when both are showing the same tab.
    [int]$Hwnd = 0,
    [int]$Pad = 16,
    [ValidateSet('Alpha', 'Screen', 'PrintWindow')][string]$Method = 'Alpha'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

$raw = Join-Path ([System.IO.Path]::GetTempPath()) 'ccdash-wt-strip.png'
$shot = @{ Method = $Method; Out = $raw }
if ($Hwnd -ne 0) {
    $shot.Hwnd = $Hwnd
} else {
    $shot.ProcessName = 'WindowsTerminal'
    $shot.First = $true
    if ($TitleLike) { $shot.TitleLike = $TitleLike }
}

# Refuse a non-publishable project before the shutter. This frame publishes a
# session name per tab, and a tab strip is the figure most likely to carry one
# nobody meant to include, because it shows whatever is in that window rather
# than what the dashboard chose to draw.
#
# THE COVERAGE IS PARTIAL AND THAT IS WORTH STATING RATHER THAN IMPLYING. The
# check reads the dashboard's roster, so it covers every tab backed by a session
# the dashboard tracks -- which is every tab this figure is about. It CANNOT see
# a tab the dashboard knows nothing about: a shell, an editor, a session whose
# hook never fired. Those carry whatever title their program set, and no roster
# lookup will reach them. So this closes the common case and the manifest's
# instruction to read the strip by eye still stands for the rest; a guard that
# silently covered less than it appeared to would be worse than none.
#
# -TitleLike is the window's own caption, which Windows Terminal sets from the
# ACTIVE tab, so it is a name in frame that the roster may not match verbatim --
# it carries a status glyph and possibly a [N%] suffix. It is passed as-is and
# checked on its own; a caption that is not a bare project name simply fails to
# match the list and is reported, which is the safe direction.
Assert-Publishable -Where 'the tab strip' -LocalOnly

Invoke-WindowShotWithoutWidget $shot | Write-Host

$bmp = [System.Drawing.Bitmap]::FromFile($raw)
try {
    # One LockBits pass instead of GetPixel: the width scan alone is well over a
    # hundred thousand samples, which takes tens of seconds through the managed
    # per-pixel API and well under one this way.
    $rect = New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $bytes = New-Object byte[] ($data.Stride * $bmp.Height)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
        $stride = $data.Stride
    } finally { $bmp.UnlockBits($data) }

    # BGRA
    function Get-Px([int]$x, [int]$y) {
        $i = $y * $stride + $x * 4
        return @($bytes[$i + 2], $bytes[$i + 1], $bytes[$i])
    }
    function Test-Differs($a, $b) {
        return ([math]::Abs($a[0] - $b[0]) -gt 10 -or [math]::Abs($a[1] - $b[1]) -gt 10 -or [math]::Abs($a[2] - $b[2]) -gt 10)
    }

    # Start below the window's own border. A maximized terminal has none, but a
    # normal one has a 1-2px frame at the very top, so sampling row 0 as the
    # strip's background measures a strip zero pixels tall. Row 8 is inside the
    # bar on both, and the crop still starts at 0 so the border stays in frame.
    $top = 8
    $bg = Get-Px 4 $top
    $stripH = 0
    for ($y = $top; $y -lt [math]::Min(200, $bmp.Height); $y++) {
        if (Test-Differs (Get-Px 4 $y) $bg) { break }
        $stripH = $y + 1
    }
    if ($stripH -lt 16) { throw "Could not find a tab strip: it measured ${stripH}px. Is the captured window Windows Terminal, and is its leftmost column empty bar?" }
    # Reaching the cap means no row differed from the bar colour, i.e. the bottom
    # edge was never found -- which happens on a light or low-contrast terminal
    # theme where the terminal background sits within tolerance of the tab row.
    # Saving that crop would ship an arbitrary 200px band as if it were the strip.
    if ($stripH -ge 200) { throw 'The tab strip has no detectable bottom edge -- the terminal background is too close to the tab-row colour to measure. Nothing was written.' }

    # Where the tabs end. The window's minimise/maximise/close buttons sit at the
    # far right of this same bar, so the last non-background column is always
    # theirs, not a tab's.
    #
    # Identify them by the one property that does not depend on how wide the
    # window is: they are flush against the right edge, and the tab strip's own
    # content never is. An earlier rule cut at the first empty run wider than
    # 400px, which worked on a maximized window (the gap there is ~2500px) and
    # silently kept the controls on a narrower one (~340px) -- so the same script
    # produced a different frame depending on the window, which is exactly what a
    # committed screenshot must not do.
    #
    # The scan runs from $top, not from row 0, for the same reason the height
    # measurement does: the window's own border occupies the first rows and
    # differs from the bar colour at *every* x, so scanning from 0 marks the
    # whole width as content and the crop then always runs to the right edge.
    $content = New-Object bool[] $bmp.Width
    for ($x = 0; $x -lt $bmp.Width; $x++) {
        for ($y = $top; $y -lt $stripH; $y++) {
            if (Test-Differs (Get-Px $x $y) $bg) { $content[$x] = $true; break }
        }
    }

    $last = -1
    for ($x = $bmp.Width - 1; $x -ge 0; $x--) { if ($content[$x]) { $last = $x; break } }
    if ($last -lt 0) { throw 'The tab strip is empty — no tab was found to photograph.' }

    $right = $last
    if ($last -ge $bmp.Width - 8) {
        # Walk left out of the flush-right cluster, over the gap before it, and
        # take the last tab-side column beyond that. The threshold has to clear
        # the gaps BETWEEN the three control buttons -- measured 54px at 144 DPI
        # -- while staying under the gap that separates them from the tabs, which
        # was 371px on a default-sized window and about 2500px maximized. Walking
        # from the right means the first qualifying gap is always that one, so
        # the wider gaps inside the tab area are never reached.
        $x = $last
        $blank = 0
        while ($x -ge 0 -and $blank -lt 120) {
            if ($content[$x]) { $blank = 0 } else { $blank++ }
            $x--
        }
        while ($x -ge 0 -and -not $content[$x]) { $x-- }
        if ($x -lt 0) { throw 'Found the window controls but no tabs to their left.' }
        $right = $x
    }

    $w = [math]::Min($bmp.Width, $right + $Pad + 1)
    $crop = $bmp.Clone((New-Object System.Drawing.Rectangle 0, 0, $w, $stripH), $bmp.PixelFormat)
    try {
        $out = Get-ShotPath 'terminal-tabs-windows'
        $crop.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
        Write-Host "$out  $($crop.Width)x$($crop.Height)  strip height ${stripH}px"

        # Checked on the crop rather than the whole window: an unrendered band is
        # inherited by whatever is cut out of it, and the crop is the frame that
        # gets committed. Its right edge is bare tab-strip grey, so an opaque
        # black column there is the fault and not the design.
        Assert-Rendered -Path $out

        # Give the crop an edge. A region crop never brings one: this keeps the
        # window's own translucent border along its top and left, and its right
        # and bottom are wherever the bounds above were measured -- bare content
        # at (46, 46, 46), which on a dark page has nothing to stop the picture.
        #
        # AFTER the save, not before, because the border has to trace the cropped
        # shape; stroking the whole window first would put the edge where this
        # crop cuts it away.
        #
        # The script is the docs-relevance skill's, shared with the macOS capture
        # rather than reimplemented here: one border, one width, one colour, on
        # two frames the README prints side by side. It decides for itself whether
        # a frame needs one, so the widget captures -- which arrive with Windows'
        # own border -- are left alone by it.
        #
        # -Opaque because this frame in particular keeps a border on two sides and
        # is cut bare on the other two, and Windows' own is translucent: left as
        # it is, the kept sides take their colour from whatever is behind the page
        # and vanish on a dark one while the stroked sides stay bright. The flag
        # replaces all four with one opaque band so the crop has a single border.
        # It is passed here rather than decided in the skill because which frames
        # are crops of a translucent-bordered window is something this project
        # knows and that script cannot see.
        Add-Hairline -Path $out -Opaque
    } finally { $crop.Dispose() }
} finally {
    $bmp.Dispose()
    Remove-Item $raw -ErrorAction SilentlyContinue
}
