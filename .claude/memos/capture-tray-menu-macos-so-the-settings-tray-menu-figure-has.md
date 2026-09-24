---
created: 2026-09-24 14:21:20
platform: macos
---

# Capture tray-menu-macos, so the Settings › Tray menu figure has its macOS half

docs/pages/settings.md › Tray menu embeds {% include figure.html id="tray-menu" %}, and only tray-menu-windows.png exists. figure.html probes screenshots/<id>-macos.png and renders whatever variants it finds, so a tray-menu-macos.png plus its own manifest entry in docs/screenshots/screenshots.json is all the wiring the pair needs — no page edit.

The macOS menu is not the Windows one: it carries 'Keep awake with lid closed' with its radio group and countdown label, which the Windows build never shows, so the figure's alt text ("The tray menu, open: Show / Hide, the checkable toggles, the submenus, ...") should be checked against the macOS frame when it lands.

Write it as docs/screenshots/capture/tray-menu-macos.py on the shared macOS lib, like the other *-macos.py scripts, and record it in capture.command. The Windows script (tray-menu-windows.ps1) opens the menu by posting tray-icon's WM_USER_TRAYICON to the tray_icon_app window; that mechanism does not exist on macOS, where the menu is an NSMenu opened through the status item, so the staging has to be worked out there — prefer an app-level route over synthesized clicks, per the docs-relevance staging ladder. Capture with DocShot like the other macOS frames. The new entry's policy is the user's to set; tray-menu-windows was set to auto on 2026-09-24, which does not decide this one.
