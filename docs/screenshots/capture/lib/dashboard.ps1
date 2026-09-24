<#
  Shared helpers for the Windows capture scripts.

  EVERY SCRIPT THAT DOTS THIS FILE DECLARES [CmdletBinding()], and it is not
  decoration. Without it a simple param block is not strict: PowerShell drops an
  argument the script does not declare into `$args` silently, with no error and
  no warning. That is how a doc comment telling the operator to probe with
  `-Method PrintWindow` produced a run that took the machine over and overwrote
  the committed frame -- the flag was accepted, ignored, and the hardcoded Alpha
  path ran anyway. With the attribute the same call fails loudly instead.

  SAVE EVERY .ps1 HERE AS UTF-8 WITH A BOM. Windows PowerShell reads a .ps1
  without one as ANSI, so a single non-ASCII character -- an em dash in a comment
  is enough -- becomes two bytes that break the parse somewhere else entirely.
  Rewriting these files with a tool that defaults to BOM-less UTF-8 has already
  done it once, and the error it produced ("Unexpected token 'is'") pointed at a
  line that was not wrong.
#>
<#
.SYNOPSIS
  Drive the running dashboard's own windows from a capture script.

.DESCRIPTION
  Dot-source this: `. "$PSScriptRoot/lib/dashboard.ps1"`.

  Every documentation frame is a window this app can be asked to open, size and
  place over its loopback API (`POST /api/window`), so a capture script states
  the window it wants rather than telling a human where to click. The API is
  deliberately a control surface and not a capture hook -- it will not scroll a
  window or frame a shot -- so anything beyond show/size/place stays in the
  per-frame script where it can be read.

  The port comes from the app's own config rather than a constant, because
  `config/local.json` is sometimes pointed at 9078 to keep a test instance off
  the live one, and a capture script that quietly talked to the wrong instance
  would produce a plausible screenshot of the wrong thing.
#>

function Get-DashboardPort {
    $cfg = Join-Path $env:APPDATA 'com.anothersava.claude-code-dashboard\config.json'
    if (Test-Path $cfg) {
        try {
            $port = (Get-Content $cfg -Raw | ConvertFrom-Json).server_port
            if ($port) { return [int]$port }
        } catch { }
    }
    return 9077
}

function Invoke-DashboardWindow {
    param([Parameter(Mandatory = $true)][hashtable]$Body)

    $port = Get-DashboardPort
    $json = $Body | ConvertTo-Json -Compress
    try {
        # No Origin header: the CSRF guard refuses every value except a literal
        # `null`, and sending a plausible-looking one is a 403.
        $r = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$port/api/window" `
            -ContentType 'application/json' -Body $json -TimeoutSec 10
    } catch {
        # Invoke-RestMethod treats any non-2xx as terminating, so a refusal lands
        # here rather than below -- which is why the reason has to be dug out of
        # the response body instead of read off $r.ok.
        $status = $_.Exception.Response.StatusCode.value__
        $reason = $null
        try { $reason = ($_.ErrorDetails.Message | ConvertFrom-Json).reason } catch { }
        if ($status) { throw "The dashboard refused $($Body.action): HTTP $status$(if ($reason) { " ($reason)" })" }
        throw "POST /api/window failed on port $port ($($Body.action)) -- no response. Is the dashboard running? $_"
    }
    if (-not $r.ok) { throw "The dashboard refused $($Body.action): $($r.reason)" }
    return $r
}

function Get-DashboardAgents {
    $port = Get-DashboardPort
    return Invoke-RestMethod -Uri "http://127.0.0.1:$port/api/agents" -TimeoutSec 10
}

# Takes a hashtable and splats it, rather than collecting remaining arguments:
# with -ValueFromRemainingArguments PowerShell resolves `-Out` against its own
# common parameters first and fails on the ambiguity with -OutVariable.
function Invoke-WindowShot {
    param([Parameter(Mandatory = $true)][hashtable]$Params)
    & (Join-Path $PSScriptRoot 'window-shot.ps1') @Params
}

# For every frame that is NOT the widget itself.
#
# The widget is always-on-top, so raising the target window does not get it out
# from under: it stays above whatever has focus, and any capture that reads
# pixels off the screen photographs it sitting inside the frame. That happened
# to the history window before this existed -- the widget was in the bottom-right
# corner of a committed shot.
#
# It goes back afterwards whatever happens, including on a failed capture.
function Invoke-WindowShotWithoutWidget {
    param([Parameter(Mandatory = $true)][hashtable]$Params)
    $hidden = $false
    try {
        Invoke-DashboardWindow @{ action = 'hide' } | Out-Null
        $hidden = $true
        Start-Sleep -Milliseconds 400
        Invoke-WindowShot $Params
    } finally {
        if ($hidden) { Invoke-DashboardWindow @{ action = 'show' } | Out-Null }
    }
}

# Draw a saved frame's Windows 11 frame again, from Windows' own model.
#
# A captured Windows frame cannot be kept or cleaned: its border is translucent, so
# it arrives mixed with the drop shadow and the backdrop behind it (the lighter-top,
# darker-bottom shading a captured border shows is the shadow, not the border).
# The docs-relevance skill's winframe.py keeps only the content inside the clip
# Windows applies and draws the frame from DWM's measured model: 8 DIP corners (4
# for a menu) flattened into chords, a 1 px antialiasing ramp, a 2 px border at 144
# DPI. Drawn with Windows' own border and the window's own shadow it reproduces a
# real capture to under one level RMS. Shelled out rather than written here for the reason every shared step
# is: one implementation for every project, and the macOS half cannot read a
# PowerShell function.
#
# THE RING IS LIGHTER THAN WINDOWS' AND THERE IS NO SHADOW, chosen by eye on
# 2026-09-24. Windows' own border, rgba(117,117,117,0.40), reads 200 on a white
# page and 55 on GitHub's dark one, where a README renders for a dark-mode reader.
# rgba(146,146,146,0.69) reads 180 and 105 on every side. The shadow is left out
# because it is what makes the bottom of a captured border darker than its top.
#
# -Kind menu gives a menu's own shape, 4 DIP corners, rather than a window's.
# -Cut names the sides of a crop that are cuts rather than the window's edges: they
# get the border straight along them and square corners.
#
# The capture records its window's DPI in the PNG and winframe.py sizes the frame
# from it. This runs once, on the raw capture; a copy of that raw is kept first.
$WindowFrameRing = '929292:0.69'
function Add-WindowFrame {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [ValidateSet('window', 'menu')][string]$Kind = 'window',
        [ValidateSet('left', 'top', 'right', 'bottom')][string[]]$Cut = @()
    )
    $winframe = Join-Path $env:USERPROFILE '.claude\skills\docs-relevance\scripts\winframe.py'
    $py = if (Get-Command python -ErrorAction SilentlyContinue) { 'python' }
          elseif (Get-Command python3 -ErrorAction SilentlyContinue) { 'python3' }
          else { $null }
    # THE FLAGS ARE BUILT ONCE AND USED FOR BOTH THE CALL AND THE SUGGESTED REMEDY,
    # because those two drifted once: the throw messages composed their own command
    # and left a flag out of it. A remedy that differs from what the code runs is
    # worse than none.
    $flags = @('--kind', $Kind, '--shadow', 'none', '--ring', $WindowFrameRing)
    if ($Cut.Count -gt 0) { $flags += @('--cut', ($Cut -join ',')) }
    # KEEP THE RAW, and before anything can throw. The frame step rewrites the file
    # in place, so without a copy the only way to try a different frame is to take
    # the shot again -- which needs the app staged and the machine taken over. And
    # a caller that stages its file in a temp directory deletes it in `finally`, so
    # a guard that threw first would lose the capture outright. The copy goes to
    # the repo's gitignored tmp/, named after the frame, and the remedy below reads
    # from it for the same reason.
    $raws = Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path 'tmp\screenshot-raws'
    New-Item -ItemType Directory -Force -Path $raws | Out-Null
    $raw = Join-Path $raws (Split-Path -Leaf $Path)
    Copy-Item -Force $Path $raw
    # The interpreter in the remedy is the one we RESOLVED, falling back to `python`
    # only in the branch that tells you to install it.
    $suggest = "$(if ($py) { $py } else { 'python' }) `"$winframe`" $($flags -join ' ') `"$raw`" --out `"$Path`""
    if (-not $py) { throw "Captured $Path but python is not on PATH, so it has no frame. The capture is kept at $raw. Install python (with numpy and scipy) and run: $suggest" }
    if (-not (Test-Path $winframe)) { throw "Captured $Path but $winframe is missing, so it has no frame. The capture is kept at $raw. Install the dotfiles and run: $suggest" }
    & $py $winframe @flags $Path
    if ($LASTEXITCODE -ne 0) { throw "winframe.py failed on $Path; the frame was not drawn. The capture is kept at $raw. Reproduce with: $suggest" }
}

# Refuse to photograph a project that is not cleared for publication.
#
# Shelled out to lib/dashboard.py rather than reimplemented here, for the reason
# that file's own list comment gives: a copy per caller is a copy to forget to
# update, and this one would be a copy per PLATFORM, which is worse — the two
# would drift silently and only the committed frame would show it. The macOS
# capture scripts import the same function; Add-WindowFrame already shells out to
# winframe.py, so the shape is proven here.
#
# THE ROSTER GOES ACROSS, NOT THE NAMES. Extracting "what is on screen" from a row
# is `names_in_frame` on the Python side, and it belongs there: it encodes what
# SessionItem.svelte draws, including the case a hand-rolled extraction gets wrong,
# where a renamed row publishes its display_name rather than its project. So this
# pipes /api/agents in verbatim and lets the shared rule decide.
#
# -Name adds strings the roster does not carry, such as a window title.
# -LocalOnly is for a frame taken with sync off, where a peer's rows are not drawn.
function Assert-Publishable {
    param(
        [string]$Where = 'the frame',
        [string[]]$Name = @(),
        [switch]$LocalOnly
    )
    $py = if (Get-Command python -ErrorAction SilentlyContinue) { 'python' }
          elseif (Get-Command python3 -ErrorAction SilentlyContinue) { 'python3' }
          else { $null }
    $lib = Join-Path $PSScriptRoot 'dashboard.py'
    $pyArgs = @($lib, 'assert-publishable', '--where', $Where)
    foreach ($n in $Name) { if ($n) { $pyArgs += @('--name', $n) } }
    if ($LocalOnly) { $pyArgs += '--local-only' }
    # `python` literally, not $py: this string is only ever read in the branch
    # where $py is null, so interpolating it would always render the fallback and
    # the conditional would be an unreachable arm pretending to be a choice.
    $suggest = "python `"$lib`" assert-publishable --where `"$Where`""
    if (-not $py) { throw "Cannot check which projects would be in $Where because python is not on PATH, and a frame publishes every project name it shows. Install python, or check by eye and re-run. Manual: $suggest" }

    $port = Get-DashboardPort
    $roster = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/agents" -TimeoutSec 10 -UseBasicParsing
    $roster.Content | & $py @pyArgs
    if ($LASTEXITCODE -ne 0) { throw "Refused to capture $Where -- see above. Nothing was written. Re-check with: $suggest" }
}

# Refuse a frame whose right edge came back unrendered.
#
# A Windows capture reads the window's own surface rather than the screen, which
# is what lets it shoot a window that is partly off-display -- but only the part
# the compositor rendered comes back. The rest is FLAT BLACK at full alpha, the
# same size as the window, so the file looks complete and the loss is silent: the
# first end-to-end run of work-intensity-windows.ps1 committed a 1522px frame
# whose last 239px were a black band where the right gutter and two controls
# should have been, and nothing in the pipeline objected.
#
# Checked on a column just inside the right edge, over the vertical middle, which
# is where the band lands: this window furniture is never pure black (the chart's
# own background is #1c1c1e), so an opaque 0,0,0 run there is unrendered surface
# and not a dark design. Windows only -- `screencapture -l` on macOS reads the
# backing store and returns the full width regardless of what is on screen.
function Assert-Rendered {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [int]$Inset = 8
    )
    Add-Type -AssemblyName System.Drawing
    $bmp = [System.Drawing.Bitmap]::FromFile($Path)
    try {
        $x = $bmp.Width - 1 - $Inset
        $y0 = [int]($bmp.Height * 0.2)
        $y1 = [int]($bmp.Height * 0.8)
        $black = 0
        for ($y = $y0; $y -lt $y1; $y++) {
            $p = $bmp.GetPixel($x, $y)
            if ($p.A -eq 255 -and $p.R -eq 0 -and $p.G -eq 0 -and $p.B -eq 0) { $black++ }
        }
        $n = $y1 - $y0
        if ($black -eq $n) {
            throw "The right edge of $Path is $n/$n opaque black, so the window was only partly rendered -- it was most likely hanging off the edge of its display. Nothing usable was written. Move the window fully onto one screen and re-run."
        }
    } finally {
        $bmp.Dispose()
    }
}

function Get-ShotPath {
    param([Parameter(Mandatory = $true)][string]$Id)
    # capture/lib -> capture -> screenshots
    return Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "$Id.png"
}

# Move the pointer to (X, Y) for as long as -Do runs and put it back afterwards,
# whatever -Do does. Two frames need the pointer somewhere in particular: a tray
# menu opens at it, and a window that opens under a resting pointer shows its
# hover tooltip, which moving the pointer away afterwards does not clear.
#
# Coordinates are physical pixels. Get-PrimaryWorkArea returns the rectangle to
# aim at in the same units: the thread is made PerMonitorV2 aware first, or a
# scaled display reports its work area divided by its scale.
if (-not ('DashboardPointer' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public class DashboardPointer {
    [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr ctx);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
'@
}

function Get-PrimaryWorkArea {
    Add-Type -AssemblyName System.Windows.Forms
    [void][DashboardPointer]::SetThreadDpiAwarenessContext([IntPtr](-4))
    return [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
}

function Invoke-WithPointerAt {
    param(
        [Parameter(Mandatory = $true)][int]$X,
        [Parameter(Mandatory = $true)][int]$Y,
        [Parameter(Mandatory = $true)][scriptblock]$Do
    )
    [void][DashboardPointer]::SetThreadDpiAwarenessContext([IntPtr](-4))
    $was = New-Object DashboardPointer+POINT
    [void][DashboardPointer]::GetCursorPos([ref]$was)
    [void][DashboardPointer]::SetCursorPos($X, $Y)
    try { & $Do } finally { [void][DashboardPointer]::SetCursorPos($was.X, $was.Y) }
}
