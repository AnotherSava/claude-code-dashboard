<#
  Windows variant of the `hero` figure — the image on the README and the docs
  index.

  Captured with -Method Alpha, so the frame carries the window as it actually
  looks: its border curving round the corners, antialiased, and everything
  outside the corner transparent. A client-area capture was tried first and is
  wrong for a documentation frame -- it drops the border and squares off the
  corners, which is neither what Windows draws nor what the macOS variants show.

  What this script cannot do is put the widget into the state the frame has to
  show: one row blocked on its user, one working, one finished. Those are real
  sessions in real states, and the steps in screenshots.json say how to stage
  them. The script checks the spread is there and refuses rather than capturing
  whatever happens to be on screen — a hero that quietly shows three idle rows
  is worse than no hero, because nothing about it looks wrong.

  THE GUARD READS A DIFFERENT SNAPSHOT THAN THE FRAME PHOTOGRAPHS, and the one
  case where they disagree has to be checked by eye. `/api/agents` is served from
  `commands::resolved_snapshot`; the widget renders `display_snapshot`, which is
  that plus `apply_read_as_idle`. So a DONE row the user has already looked at is
  still `done` here and draws as IDLE there — the attention stamp is this
  machine's observation of its own keyboard and is deliberately never on the
  wire, so no HTTP question can distinguish the two. Staging a DONE session and
  then clicking through its terminal tab is exactly how to produce it. Make the
  staged session emit something fresh if the captured frame shows IDLE where the
  guard said DONE.

  The guard also runs twice — before the window is raised and again after the
  shutter — because a real session can change state inside the second and a half
  in between, and the second pass is what turns that into a refusal rather than a
  committed frame nobody re-examines.
#>
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

function Assert-HeroSpread([string]$When) {
    $agents = Get-DashboardAgents
    $local = @($agents.agents | Where-Object { $_.local })
    $states = @($local | ForEach-Object { $_.status } | Sort-Object -Unique)
    $want = @('blocked', 'done', 'working')
    $missing = @($want | Where-Object { $states -notcontains $_ })

    if ($missing.Count -gt 0) {
        Write-Host "Local rows ${When}: $(($local | ForEach-Object { "$($_.id)=$($_.status)" }) -join ', ')"
        throw "The hero needs one row in each of blocked/working/done; missing $When`: $($missing -join ', '). Stage them (see screenshots.json) or pass -Force."
    }
    if ($agents.peers.Count -gt 0) {
        throw "A peer is connected ($(($agents.peers | ForEach-Object { $_.device }) -join ', ')), so the widget is showing its rows too. Turn sync listening off for this frame, or pass -Force."
    }
}

if (-not $Force) { Assert-HeroSpread 'before the shot' }

Invoke-DashboardWindow @{ action = 'show' } | Out-Null
Start-Sleep -Milliseconds 1200

$out = Get-ShotPath 'hero-windows'
Invoke-WindowShot @{ ProcessName = 'claude-code-dashboard'; Title = 'Claude Code Dashboard'; Method = 'Alpha'; Out = $out }

if (-not $Force) { Assert-HeroSpread 'after the shot' }
