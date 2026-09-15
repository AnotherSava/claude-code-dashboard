---
created: 2026-09-15 05:21:43
---

# Re-shoot work-intensity-macos once this change set is pushed: it still shows the removed Percent|Tokens switch

Blocked on the push: the Mac has to pull and deploy the token-only chart before its capture script can photograph it.

The committed macOS frame shows the Percent|Tokens switch, the dashed full-5h-pace reference line and two stat lines per row. The chart now has none of those, and three lines per row — active time, tokens, and the day's share of the weekly quota. Its screenshots.json entry already carries a "STALE — RE-SHOOT" step and its verifiedAt has been dropped, so check-figures.py will not flag it and nothing else will either.

Move the subject week to Aug 31 – Sep 6 2026 while re-shooting, to match the Windows half, which is re-shot and committed; the macOS frame still shows Aug 10 – Aug 16. A paired figure is read as two pictures of one thing.

Two harness fixes made on the Windows side are worth checking on the Mac. The capture now places the window on the primary display before sizing it, because a window hanging off a display comes back as unrendered black at the full window size in a Windows capture — screencapture -l reads the backing store, so that may not apply there. And lib/dashboard.ps1 gained Assert-Rendered, which refuses a frame whose right edge is opaque black; work-intensity-macos.py has no counterpart.

assert_percent_unit is already deleted from work-intensity-macos.py, so nothing in the script refuses the run.

Related: [[rework-the-docs-screenshots-the-windows-half-is-done-the]].
