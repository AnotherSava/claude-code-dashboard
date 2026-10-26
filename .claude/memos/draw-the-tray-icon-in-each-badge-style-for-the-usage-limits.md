---
created: 2026-10-25 21:39:32
---

# Draw the tray icon in each badge style for the Usage limits section of the features page

docs/pages/features.md (Usage limits) spends three sentences describing how the tray icon looks in each tray_badge style: the traffic-light recolour, the two-digit number, and the red context-alert variants of both (the outline on the icon styles, the red plate behind the digits). A figure would replace that description. It needs no app running: tray_badge.rs's render functions (render_light_badge, render_badge_alert, draw_alert_border, render_plain_icon) can draw every variant to PNGs, the way the debug_preview_tray_rendering project memory describes (a throwaway #[ignore] test with the png dev-dependency). Next step: write that render as a capture script under docs/screenshots/capture/, add a manifest entry (no policy; the user sets it), frame the strip, and embed it in features.md. Raised by screenshot-relevance on 2026-10-25 as gap G3.
