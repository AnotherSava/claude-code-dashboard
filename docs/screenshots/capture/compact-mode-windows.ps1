<#
  Windows variant of the `compact-mode` figure.

  Compact view is a config setting rather than a window state, so this script
  writes it, captures, and puts it back. Two things about that write are not
  optional:

  * The file is written whole and then touched, rather than truncated in place.
    The config watcher reads on the FIRST notify event and then debounces for
    150ms, so a truncate-then-write hands it a half-written file, it falls back
    to defaults across the board, and the real write is swallowed by the
    debounce. Writing the complete file, waiting past the debounce and touching
    it gives the watcher a second event against a file that is already correct.
  * The restore runs in `finally`. A capture that fails partway through must not
    leave the user's widget in a mode they did not choose.

  As with the hero, -Method Alpha: the window's content and its rounded outline,
  transparent outside it. The frame itself is then drawn by Add-WindowFrame.
#>
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

$cfgPath = Join-Path $env:APPDATA 'com.anothersava.claude-code-dashboard\config.json'
if (-not (Test-Path $cfgPath)) { throw "No config at $cfgPath — is the dashboard installed?" }

function Set-CompactMode([bool]$on) {
    $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
    $cfg.compact_mode = $on
    $cfg | ConvertTo-Json -Depth 20 | Set-Content $cfgPath -Encoding utf8
    Start-Sleep -Milliseconds 700          # clear the watcher's 150ms debounce
    (Get-Item $cfgPath).LastWriteTime = Get-Date
    Start-Sleep -Milliseconds 1500
}

$was = [bool]((Get-Content $cfgPath -Raw | ConvertFrom-Json).compact_mode)

$agents = Get-DashboardAgents
if ($agents.peers.Count -gt 0 -and -not $Force) {
    throw "A peer is connected ($(($agents.peers | ForEach-Object { $_.device }) -join ', ')), so the widget is showing its rows too. Decide whether this frame should carry device pills before capturing it — see screenshots.json — or pass -Force."
}

try {
    Set-CompactMode $true
    Invoke-DashboardWindow @{ action = 'show' } | Out-Null
    Start-Sleep -Milliseconds 1200
    Assert-Publishable -Where 'the compact-mode frame' -LocalOnly
    $out = Get-ShotPath 'compact-mode-windows'
    Invoke-WindowShot @{ ProcessName = 'claude-code-dashboard'; Title = 'Claude Code Dashboard'; Method = 'Alpha'; Out = $out }
    Assert-Rendered -Path $out
    Add-WindowFrame -Path $out
} finally {
    Set-CompactMode $was
}
