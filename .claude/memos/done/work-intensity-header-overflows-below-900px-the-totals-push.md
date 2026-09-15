---
created: 2026-09-01 19:37:00
---

# Work intensity header overflows below ~900px: the totals push the Days/Weeks switch off the window

FIXED 2026-09-15 by `minWidth: 1000` on the intensity window in
`src-tauri/tauri.conf.json`, which is below the width the header needs and above
the width at which it breaks, so the broken state is no longer reachable by
dragging. Verified by reading the window's own `WM_GETMINMAXINFO` back:
`ptMinTrackSize` is 1522x716 physical, 1000x440 logical at this machine's 150%.
A programmatic `set_size` still goes below it — Windows applies the minimum to
user-driven sizing only — which is how the measurements below were taken.

What it was: `.totals` is `white-space: nowrap` and the `.switch` beside it is
`flex: none`, so once the stats no longer fit, the switch is pushed off the
right edge and the last total is clipped mid-word. Nothing wrapped, nothing
truncated with an ellipsis; it simply left the window.

The break width moved three times in one afternoon, which is why pinning it was
worth more than chasing it: 1280 with the Navigation and Legend hints in the
header, about 850 once those came out, and just under 1000 after each stat's
value got a reserved width so the header would stop shifting when the view
switches. The reservations are what made the fix necessary and sufficient at the
same number.
