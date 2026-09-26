<#
  Windows variant of the `hero` figure — the image on the README and the docs
  index.

  The rows come from fixtures/hero.json, replayed by Invoke-DashboardFixture: the
  local ones through the dashboard's own /api/event, so the classifier, the state
  machine and the widget run their production code, and AIR's pushed to its sync
  listener as that peer would push them. The projects are the author's public
  repositories and every string is written in the fixture, which keeps private
  names and prompts out of a published image; the guard after the shutter refuses
  a frame in which any other row appeared. The replay takes a few minutes, because
  a local row's timer can only start when its events arrive.

  The shot is retaken until both BLOCK pills are near full brightness, since they
  pulse; the fixture blocks the two rows a whole number of pulses apart so they
  peak together.

  Captured with -Method Alpha, so the frame carries the window as it actually
  looks: its border curving round the corners, antialiased, and everything
  outside the corner transparent. A client-area capture drops the border and
  squares off the corners, which is neither what Windows draws nor what the macOS
  variants show.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

$fixture = Join-Path $PSScriptRoot 'fixtures/hero.json'
Invoke-DashboardFixture -Fixture $fixture -Do {
    Invoke-DashboardWindow @{ action = 'show' } | Out-Null
    Start-Sleep -Milliseconds 1200

    $out = Get-ShotPath 'hero-windows'
    Invoke-WindowShotAtPulsePeak @{ ProcessName = 'claude-code-dashboard'; Title = 'Claude Code Dashboard'; Method = 'Alpha'; Out = $out }
    Assert-Rendered -Path $out
    Add-WindowFrame -Path $out
    Assert-FixtureShown $fixture
}
