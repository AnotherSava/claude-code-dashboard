---
created: 2026-09-26 01:03:40
platform: macos
---

# Port the hero fixture replay to macOS, so hero-macos is shot from fixtures/hero.json like its Windows half

hero-windows.png is now staged from a fixture (commit c9012e9): docs/screenshots/capture/fixtures/hero.json replays local rows through the dashboard's own /api/event and pushes AIR's rows to the sync listener as that peer, so the frame shows every status, a high-context row, varied timers and two synced rows, with no live session in it. hero-macos.png is still shot from live sessions, so the README's two-column pair shows two different subjects.

What blocks it: fixture_up in docs/screenshots/capture/lib/dashboard.py needs to stop and restart the app on a capture config and restore it afterwards, and the app-control helpers it uses (_app_exe, _stop_app, _start_app) are Windows-only. APP_DATA is already platform-aware; capture_config, post_event, replay, push_peer_rows, check_shown, fixture_down and pulse-check are not Windows-specific.

Next step, on the Mac: implement macOS _stop_app/_start_app (the installed .app bundle; quit and relaunch), run fixture-up/fixture-check/fixture-down by hand, then make the macOS hero capture script replay hero.json the way hero-windows.ps1 does (Invoke-DashboardFixture -> pulse-peak shot -> frame). The fixture's AIR rows would then be pushed by the Mac as a peer called AIR to itself, so pick a device name for those rows that reads right on a Mac-captured frame (e.g. CHROME) or make the device a fixture parameter. The manifest's hero-windows 'The pair with hero-macos' step records the same gap; update it when done.
