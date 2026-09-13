<#
  Windows variant of the `work-intensity` figure.

  Opens the Work intensity window on a chosen week, sizes it wide enough that the
  header stays on one line, and captures it with its native title bar.

  THE SUBJECT IS A WEEK, AND IT IS PINNED BY DATE: Aug 31 - Sep 6, 2026, the week
  this figure has always shown. A re-shoot keeps it. The chart is the one figure
  here whose content is a moving window over live data, so re-running this script
  next month with the same arguments would quietly photograph a different week --
  and the prose beside the figure, which talks about the pace line and the red
  over-2x bars, was written against this one.

  That is why the week is a DATE and not the -Offset the window's API actually
  takes. An offset is relative to whenever the script runs, so it names a
  different week every week while looking like a constant; -WeekStart is resolved
  against today at run time and the offset is derived from it. Pass -Offset only
  to look around, and -List to see what is worth looking at: the frame has to
  argue something, and the pace reference line and the over-2x red bars only show
  up in a week that was actually busy.

  The macOS half of this figure still shows Aug 10 - Aug 16 and should move to
  this week when it is re-shot -- a paired figure is read as two pictures of one
  thing, and two different weeks make it a puzzle instead of a comparison.

  Width is 1520 logical px. Below roughly 1280 the header controls collide (a
  known open defect); at 1320 they merely wrap onto two lines. 1520 is the first
  width where "Navigation" and "Legend" sit on the same line as the date range.
#>
[CmdletBinding()]
param(
    [string]$WeekStart = '2026-08-31',
    [int]$Offset,
    [int]$Width = 1520,
    [int]$Height = 700,
    [switch]$List
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

if ($List) {
    Write-Host 'Open the window by hand and step back with the arrow keys; the header prints each week''s "Active" total.'
    exit 0
}

if (-not $PSBoundParameters.ContainsKey('Offset')) {
    # Weeks run Monday to Sunday in local time, matching `local_week_start_ms`.
    $thisMonday = (Get-Date).Date.AddDays(-(([int](Get-Date).DayOfWeek + 6) % 7))
    $wantMonday = ([datetime]::ParseExact($WeekStart, 'yyyy-MM-dd', $null)).Date
    if ($wantMonday.DayOfWeek -ne [DayOfWeek]::Monday) {
        throw "-WeekStart must be a Monday; $WeekStart is a $($wantMonday.DayOfWeek)."
    }
    $Offset = [int][math]::Round(($wantMonday - $thisMonday).TotalDays / 7)
    Write-Host "Week $WeekStart resolves to offset $Offset from the week of $($thisMonday.ToString('yyyy-MM-dd'))."
    if ($Offset -gt 0) { throw "-WeekStart $WeekStart is in the future." }
}

Invoke-DashboardWindow @{ action = 'intensity'; offset = $Offset; view = 'day' } | Out-Null
Start-Sleep -Milliseconds 800
Invoke-DashboardWindow @{ action = 'resize'; label = 'intensity'; width = $Width; height = $Height } | Out-Null
# The chart animates its bars in, and the window has to settle at the new size
# before the header decides whether it fits on one line.
Start-Sleep -Milliseconds 2500

$out = Get-ShotPath 'work-intensity-windows'
Invoke-WindowShotWithoutWidget @{ ProcessName = 'claude-code-dashboard'; Title = 'Work intensity'; Method = 'Alpha'; Out = $out }
Add-Hairline -Path $out -Opaque
