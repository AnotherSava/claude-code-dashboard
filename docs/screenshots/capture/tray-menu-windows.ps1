<#
  Windows variant of the `tray-menu` figure: the tray icon's menu, open.

  The menu is a native popup, which the app's own /api/window cannot open, so
  this posts the tray window the message the shell posts on a right-click. That
  runs the crate's real TrackPopupMenu path, so the frame holds the menu a user
  gets rather than a copy of it. Three things this relies on belong to the
  tray-icon crate; re-check them in src/platform_impl/windows/mod.rs after a
  bump: the message id WM_USER_TRAYICON (6002), the window class
  `tray_icon_app`, and that the icon is registered without NIM_SETVERSION, so
  lParam carries the bare WM_RBUTTONUP the crate's handler matches on.

  TrackPopupMenu opens the menu at the pointer, so the pointer is placed first,
  near the tray corner of the primary display, and put back afterwards. The
  menu is closed in `finally`, however the capture ends: WM_CANCELMODE to its
  owner, the documented way to end another thread's menu, then Escape posted to
  the menu itself if that did not close it. Both are posted rather than typed
  because a posted message is not input, so the dashboard need not hold the
  foreground while its menu is open, and a key pressed on the keyboard would
  then go to whatever does.

  -Method Alpha with -Popup: photographed over a black and a white backdrop that
  never take focus, since anything taking focus ends the menu.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lib/dashboard.ps1')
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;

public class TrayMenuShot {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    public delegate bool EnumProc(IntPtr h, IntPtr p);
}
'@

$WM_USER_TRAYICON = 6002
$WM_RBUTTONUP = 0x0205
$WM_CANCELMODE = 0x001F
$WM_KEYDOWN = 0x0100
$WM_KEYUP = 0x0101
$VK_ESCAPE = 0x1B

$app = @(Get-Process -Name 'claude-code-dashboard' -ErrorAction SilentlyContinue)
if ($app.Count -ne 1) { throw "Expected one claude-code-dashboard process, found $($app.Count)." }
$appPid = $app[0].Id

# Every window of one class that belongs to the dashboard. The tray window is
# hidden, so visibility is a filter only when asked for.
function Find-AppWindows([string]$class, [bool]$visibleOnly) {
    $found = New-Object System.Collections.ArrayList
    $cb = [TrayMenuShot+EnumProc] {
        param($h, $p)
        $owner = 0
        [void][TrayMenuShot]::GetWindowThreadProcessId($h, [ref]$owner)
        if ($owner -eq $appPid -and (-not $visibleOnly -or [TrayMenuShot]::IsWindowVisible($h))) {
            $sb = New-Object System.Text.StringBuilder 64
            [void][TrayMenuShot]::GetClassNameW($h, $sb, $sb.Capacity)
            if ($sb.ToString() -eq $class) { [void]$found.Add($h) }
        }
        return $true
    }
    [void][TrayMenuShot]::EnumWindows($cb, [IntPtr]::Zero)
    return @($found)
}

# The dashboard's open menu windows, once they are gone or two seconds have passed.
function Wait-MenuClosed {
    for ($i = 0; $i -lt 20; $i++) {
        $left = @(Find-AppWindows '#32768' $true)
        if ($left.Count -eq 0) { return @() }
        Start-Sleep -Milliseconds 100
    }
    return $left
}

$tray = @(Find-AppWindows 'tray_icon_app' $false)
if ($tray.Count -ne 1) { throw "Expected one tray_icon_app window in the dashboard, found $($tray.Count)." }
if (@(Find-AppWindows '#32768' $true).Count -gt 0) { throw 'A dashboard menu is already open. Close it and re-run.' }

$area = Get-PrimaryWorkArea
Invoke-WithPointerAt -X ($area.Right - 40) -Y ($area.Bottom - 8) -Do {
    try {
        [void][TrayMenuShot]::PostMessageW($tray[0], $WM_USER_TRAYICON, [IntPtr]1, [IntPtr]$WM_RBUTTONUP)
        $menu = @()
        for ($i = 0; $i -lt 30 -and $menu.Count -eq 0; $i++) {
            Start-Sleep -Milliseconds 100
            $menu = @(Find-AppWindows '#32768' $true)
        }
        if ($menu.Count -ne 1) { throw "The tray menu did not open: $($menu.Count) menu windows after 3s." }
        # The entrance animation runs about 250ms; the two exposures must not
        # straddle it, or the solve records the fade as translucency.
        Start-Sleep -Milliseconds 600

        $out = Get-ShotPath 'tray-menu-windows'
        Invoke-WindowShot @{ Hwnd = $menu[0].ToInt32(); Method = 'Alpha'; Popup = $true; Out = $out }
        # A menu's own shape, 4 DIP corners, with the same ring as every window frame.
        Add-WindowFrame -Path $out -Kind menu
    } finally {
        [void][TrayMenuShot]::PostMessageW($tray[0], $WM_CANCELMODE, [IntPtr]::Zero, [IntPtr]::Zero)
        $open = @(Wait-MenuClosed)
        foreach ($m in $open) {
            [void][TrayMenuShot]::PostMessageW($m, $WM_KEYDOWN, [IntPtr]$VK_ESCAPE, [IntPtr]::Zero)
            [void][TrayMenuShot]::PostMessageW($m, $WM_KEYUP, [IntPtr]$VK_ESCAPE, [IntPtr]::Zero)
        }
        if ($open.Count -gt 0) { $open = @(Wait-MenuClosed) }
        if ($open.Count -gt 0) { Write-Warning 'The tray menu is still open after WM_CANCELMODE and a posted Escape; close it by hand.' }
    }
}
