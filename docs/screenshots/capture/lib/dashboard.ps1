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

  The shutter (window-shot.ps1) and the helpers any project needs unchanged --
  the frame step, pointer placement, finding a process's windows -- come from
  the docs-relevance skill's windows-capture.ps1, dot-sourced below, which every
  project's Windows capture scripts share so a fix to one reaches all of them.
  What stays here is what only this app needs, such as its loopback API and
  getting its always-on-top widget out of the frame.
#>

$sharedCapture = Join-Path $env:USERPROFILE '.claude\skills\docs-relevance\scripts\windows-capture.ps1'
if (-not (Test-Path $sharedCapture)) { throw "$sharedCapture is missing, and it holds the shutter every capture here uses. Install or pull the dotfiles and re-run. If the skill was renamed there, update this path and SKILL_SCRIPTS in lib/dashboard.py to match. Nothing was staged." }
. $sharedCapture

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

# The Python that lib/dashboard.py runs under, or $null when there is none.
function Get-Python {
    if (Get-Command python -ErrorAction SilentlyContinue) { return 'python' }
    if (Get-Command python3 -ErrorAction SilentlyContinue) { return 'python3' }
    return $null
}

# Show a fixture's rows in the dashboard for as long as -Do runs, then put the
# dashboard back. The fixtures section of dashboard.py says what is pinned for the
# run, how the rows are replayed and what is restored. fixture-down runs in
# `finally`, so a capture that fails still leaves the dashboard on its own config.
function Invoke-DashboardFixture {
    param(
        [Parameter(Mandatory = $true)][string]$Fixture,
        [Parameter(Mandatory = $true)][scriptblock]$Do
    )
    $py = Get-Python
    if (-not $py) { throw "A fixture capture drives the dashboard through lib/dashboard.py, and python is not on PATH." }
    $lib = Join-Path $PSScriptRoot 'dashboard.py'
    & $py $lib fixture-up $Fixture
    if ($LASTEXITCODE -ne 0) {
        # fixture-up records what to restore before it changes anything, so a
        # failure partway through still has something to put back.
        & $py $lib fixture-down
        throw "fixture-up failed for $Fixture (see above); fixture-down was run to undo whatever it changed."
    }
    try {
        & $Do
    } finally {
        & $py $lib fixture-down
        if ($LASTEXITCODE -ne 0) { Write-Warning "fixture-down failed; the dashboard may still be on the capture config. Run: $py `"$lib`" fixture-down" }
    }
}

# Take the shot, retaking it until every BLOCK pill in it is near full brightness.
#
# A BLOCK pill pulses between full and 45% opacity every 1.6s, so a single shot
# catches it wherever the shutter lands. dashboard.py pulse-check measures each
# pill in the result; this retries until the dimmest reads at least -MinOpacity,
# or throws once -TimeoutSec has passed. Each attempt raises the window over the
# backdrops again, so the timeout also bounds how long the screen is taken.
function Invoke-WindowShotAtPulsePeak {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Params,
        [double]$MinOpacity = 0.9,
        [int]$TimeoutSec = 60
    )
    $lib = Join-Path $PSScriptRoot 'dashboard.py'
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        Invoke-WindowShot $Params
        & (Get-Python) $lib pulse-check $Params.Out --min $MinOpacity
        if ($LASTEXITCODE -eq 0) { return }
        if ((Get-Date) -gt $deadline) { throw "No shot caught every BLOCK pill at $MinOpacity opacity or more within ${TimeoutSec}s; the last one is at $($Params.Out)." }
    }
}

# Refuse unless the dashboard still shows exactly the fixture's rows. Run after the
# shutter: a live session acting during the capture puts its own row in frame.
function Assert-FixtureShown {
    param([Parameter(Mandatory = $true)][string]$Fixture)
    & (Get-Python) (Join-Path $PSScriptRoot 'dashboard.py') fixture-check $Fixture
    if ($LASTEXITCODE -ne 0) { throw "The dashboard stopped showing exactly the fixture during the capture (see above), so the frame is not usable." }
}

# Refuse to photograph a project that is not cleared for publication.
#
# Shelled out to lib/dashboard.py rather than reimplemented here, for the reason
# that file's own list comment gives: a copy per caller is a copy to forget to
# update, and this one would be a copy per PLATFORM, which is worse — the two
# would drift silently and only the committed frame would show it. The macOS
# capture scripts import the same function; the shared Add-WindowFrame already
# shells out to winframe.py, so the shape is proven on this machine.
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
    $py = Get-Python
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
