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

# Give a saved frame the documentation set's shared border.
#
# Shelled out to the documentation skill's hairline.py rather than implemented
# here, and for the same reason window-shot.ps1 already shells out to
# trim_halo.py: the macOS half of this project is Python and cannot read a
# PowerShell function, so a shared script is the only place the two platforms
# can hold ONE implementation. The alternative is the same border written twice,
# drifting in width or colour on two frames the README prints side by side.
#
# -Opaque REPLACES a translucent border rather than tinting it, and every Windows
# frame here needs it. Windows draws its window frames translucent (alpha 119 on
# the undecorated widget, ~130 on a decorated window), so a border left as the OS
# drew it takes its shade from whatever is behind the page: measured, those frames
# read 159-183 on a white page and 53-54 on a dark one, against a flat 189 on
# every macOS frame. GitHub renders a README in dark mode, so that is not a corner
# case. With -Opaque all ten frames measure 2px of #BDBDBD at full alpha and read
# 189 on both.
#
# This is deliberately NOT folded into Invoke-WindowShot. terminal-tabs-windows
# captures to a temp file and crops it before saving, so a stroke applied at shot
# time would land where that crop cuts it away AND be stroked again afterwards,
# giving a doubled edge on the two sides that keep the window's own border. The
# caller knows whether it is saving a window or a crop; the shot helper does not.
function Add-Hairline {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$Opaque
    )
    $hairline = Join-Path $env:USERPROFILE '.claude\skills\documentation\scripts\hairline.py'
    $py = if (Get-Command python -ErrorAction SilentlyContinue) { 'python' }
          elseif (Get-Command python3 -ErrorAction SilentlyContinue) { 'python3' }
          else { $null }
    # --require turns a SKIP into a failure. Without it, hairline.py's "this frame
    # already has an edge" path exits 0, which is indistinguishable from success to
    # this function -- so a frame whose border was never applied ships silently. The
    # caller only reaches here when it wants a border, so being left alone is only
    # acceptable if the edge already present is the one we would have drawn, and
    # --require is what checks that rather than assuming it.
    #
    # THE FLAGS ARE BUILT ONCE AND USED FOR BOTH THE CALL AND THE SUGGESTED REMEDY,
    # because those two drifted: the throw messages below used to compose their own
    # command and omitted --require from it. Someone who hit a guard and copied the
    # suggested fix would then run the version WITHOUT the check -- and on a frame
    # that trips has_own_edge would get "already has an edge of its own", exit 0,
    # and an off-shade border, which is the precise failure --require exists to
    # stop. A remedy that differs from what the code runs is worse than none.
    $flags = @('--require')
    if ($Opaque) { $flags += '--opaque' }
    $suggest = "python `"$hairline`" $($flags -join ' ') `"$Path`""
    if (-not $py) { throw "Saved $Path but python is not on PATH, so it has no border. Install python or run: $suggest" }
    if (-not (Test-Path $hairline)) { throw "Saved $Path but $hairline is missing. The capture scripts call the documentation skill's shared tooling; install the dotfiles and run: $suggest" }
    & $py $hairline @flags $Path
    if ($LASTEXITCODE -ne 0) { throw "hairline.py failed on $Path; the frame has no border, or the wrong one. Reproduce with: $suggest" }
}

function Get-ShotPath {
    param([Parameter(Mandatory = $true)][string]$Id)
    # capture/lib -> capture -> screenshots
    return Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "$Id.png"
}
