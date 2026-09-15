---
created: 2026-09-15 06:46:29
---

# Measure the Work intensity header's break width on macOS and set minWidth from it, so the window can be pulled in tight

The minimum is there to keep the Days/Weeks switch from being pushed off the right edge — `.totals` is `white-space: nowrap` and `.switch` is `flex: none`, so the header cannot wrap or truncate, it just loses the control.

It is currently 1000 in `tauri.conf.json`, which is the window's own default width, so the window can never be made smaller than it opens. On Windows at 150% the switch is whole from 940 up and cut at 930, so 1000 carries about 60px of slack it does not need. The slack was left deliberately, but for the wrong reason — it was justified as 'shrink room buys nothing', and the point is not room, it is that the header reads better pulled in: the `.spacer` between the totals and the switch takes every spare pixel, so at 1000 there is a ~230pt gap sitting in the middle of the header, and every pixel off the minimum closes it.

What to do: measure the break on macOS the same way — a programmatic `set_size` through `POST /api/window {"action":"resize","label":"intensity",...}` goes below the minimum (the OS applies it to user-driven sizing only), so sweep widths, capture, and find the first one where the switch renders whole. The two platforms lay this header out in different fonts, which is exactly why the Windows number cannot stand in for it.

Then set each platform's own minimum: Tauri takes per-platform overrides in `tauri.macos.conf.json` and `tauri.windows.conf.json` beside the main config, so this does not have to be one number that fits the worse case.

Windows numbers were taken 2026-09-15, after the header had been reworked — hints removed, the three totals put under one scope, Active rounded to whole hours. Any of those moving moves the break, so re-measure rather than trusting the number.
