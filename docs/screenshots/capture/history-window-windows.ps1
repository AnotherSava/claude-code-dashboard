<#
  Windows variant of the `history-window` figure.

  Opens the History window on one session and captures it with its native title
  bar. The window's title is the session's display name, not the word "History",
  so the shot is matched on the name this script just asked for.

  -Session is the row id as `GET /api/agents` reports it. The frame has to show a
  reply folded behind a `<...>` button, which only happens on a session with a
  long enough answer in its history, so the id is a parameter and the script
  refuses one the dashboard does not know rather than capturing an empty window.

  THE SUBJECT IS FIXED: the bga-assistant session, which is what
  history-window-macos shows. The two variants are read as one figure, so they
  photograph the same conversation; a re-shoot that picks whatever session is
  handy turns the pair into two unrelated pictures. Its row id here is
  `assistant` -- the basename, since `projects_root` is unset on this machine --
  and the window title is its custom name, `bga-assistant`.

  Choosing a session at all is a content question no flag can answer, so this is
  what the choice had to satisfy, kept for the day the subject must move: the
  project must be public (the row id is the window title), the dialog must be
  publishable to its last visible line, and it must be long enough to fold a
  reply. The list scrolls to the BOTTOM on open, so only the tail is in frame --
  which is what makes a session used for staging other screenshots the worst
  choice, its tail being the staging prompts. Read the tail before capturing;
  `prompt_history.json` in the app data dir holds it and is far quicker to search
  than opening windows.

  -Height IS THE SCROLL POSITION, which is the other half of the subject. The list
  is bottom-anchored and cannot be scrolled without driving the machine, so the
  window's height is what decides where the visible history starts: shrink it and
  the top of the frame moves down the conversation. 310 is measured, and it is
  measured against content rather than chosen round -- one line higher and the
  frame carries an absolute path from a reply, one line lower and the `<...>`
  fold that the figure exists to show drops out of frame.

  Probe with -Method PrintWindow, which needs no foreground and therefore no
  permission to take over the machine, until the frame is right; then take the
  Alpha shot once. A probe writes to `tmp/` and never to the committed frame, so
  iterating on the height cannot leave a half-tuned picture in the repo.
#>
[CmdletBinding()]
param(
    [string]$Session = 'assistant',
    [int]$Width = 900,
    [int]$Height = 310,
    [ValidateSet('Alpha', 'PrintWindow')]
    [string]$Method = 'Alpha'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')

$agents = Get-DashboardAgents
$row = $agents.agents | Where-Object { $_.id -eq $Session } | Select-Object -First 1
if (-not $row) {
    $known = ($agents.agents | ForEach-Object { $_.id }) -join ', '
    throw "No row with id '$Session'. Live rows: $known"
}
$title = if ($row.display_name) { $row.display_name } else { $row.id }

Assert-Publishable -Where "the history window's title bar, with that session's conversation under it" -Name $title
Invoke-DashboardWindow @{ action = 'history'; id = $Session } | Out-Null
# The window opens maximized and then pulls its dialog in; resizing before that
# lands leaves the text reflowing under the capture.
Start-Sleep -Milliseconds 2000
Invoke-DashboardWindow @{ action = 'resize'; label = 'history'; width = $Width; height = $Height } | Out-Null
Start-Sleep -Milliseconds 1500

$out = if ($Method -eq 'Alpha') { Get-ShotPath 'history-window-windows' }
       else { Join-Path (Split-Path $PSScriptRoot -Parent | Split-Path -Parent | Split-Path -Parent) 'tmp/history-window-probe.png' }
Invoke-WindowShotWithoutWidget @{ ProcessName = 'claude-code-dashboard'; Title = $title; Method = $Method; Out = $out }
# Only the Alpha path writes the committed frame; a PrintWindow probe goes to a
# scratch file and must not be dressed up to look like one.
if ($Method -eq 'Alpha') { Add-Hairline -Path $out -Opaque }

# Put the window back the way the user finds it. `save_window_position` is on by
# default, and lib.rs writes `history_window_position` when the history window is
# closed -- so without this the 310px capture height becomes the size every later
# click on a row opens, a sliver instead of the maximized window `open_history`
# gives when nothing is saved. Maximizing restores exactly that default.
Invoke-DashboardWindow @{ action = 'maximize'; label = 'history' } | Out-Null
