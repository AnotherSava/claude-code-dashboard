---
created: 2026-09-15 05:21:43
---

# Re-shoot work-intensity-macos once this change set is pushed: it still shows the removed Percent|Tokens switch

Blocked on the push: the Mac has to pull and deploy the token-only chart before its capture script can photograph it.

The committed macOS frame shows the Percent|Tokens switch, the dashed full-5h-pace reference line and two stat lines per row. The chart now has none of those, and three lines per row — active time, tokens, and the day's share of the weekly quota. Its screenshots.json entry carries a "STALE — RE-SHOOT" step, and verifiedAt was stamped at 68c3525 once the frame was actually opened — examined and found stale, which is what that field records. check-figures.py does not flag any of this, and nothing else will either.

Keep the subject week — the frame already shows Aug 31 – Sep 6 2026, the week the Windows half shows, verified by opening it on 2026-09-17. An earlier note in the capture script claimed it was still on Aug 10 – Aug 16 and was simply wrong; do not "fix" the week.

Two harness fixes made on the Windows side are worth checking on the Mac. The capture now places the window on the primary display before sizing it, because a window hanging off a display comes back as unrendered black at the full window size in a Windows capture — screencapture -l reads the backing store, so that may not apply there. And lib/dashboard.ps1 gained Assert-Rendered, which refuses a frame whose right edge is opaque black; work-intensity-macos.py has no counterpart.

assert_percent_unit is already deleted from work-intensity-macos.py, so nothing in the script refuses the run.

Related: [[rework-the-docs-screenshots-the-windows-half-is-done-the]].
