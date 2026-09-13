---
created: 2026-09-13 11:20:00
---

# The Windows capture scripts have never been run end to end since they were rewritten

Six `.ps1` files under `docs/screenshots/capture/` were edited across 2026-09-11..13 — `Add-Hairline` was added to `lib/dashboard.ps1` and wired into all five capture scripts — and **not one of them has produced a frame since**. What has been done is everything short of that: parse-checked twice under both Windows PowerShell 5.1 and pwsh 7, confirmed all five callers dot-source the lib so `Add-Hairline` resolves, the function exercised against a nonexistent path so its guards and its `$suggest` string were seen to render, and `hairline.py` itself run directly on committed blobs (refusal case, idempotency, the `--opaque --require` path, the PNG round-trip). All passed.

None of that runs the capture. So these remain unobserved on Windows:

- `Add-Hairline -Opaque` stroking a **freshly captured** frame rather than a committed blob — the real path takes `Invoke-WindowShot`'s output, which is alpha-keyed from two exposures, not a PNG read off disk.
- The `terminal-tabs` ordering, where the stroke must land on the **crop** and not on the window it was cropped from.
- `history-window-windows.ps1`'s `if ($Method -eq 'Alpha')` guard, which is supposed to keep a PrintWindow probe from being dressed up as a committed frame.
- Whether the committed Windows frames are actually **reproducible** by these scripts. They are not currently evidence of it: every one was re-processed on the Mac (192ef92) rather than produced by the Windows capture path. The script and the artifact have never met.

**Why this is not just "run it once".** A capture takes the machine — it hides the always-on-top widget and reads the strip off the desktop — so it needs the user's approval at the moment it runs, and it re-shoots a committed figure with whatever sessions happen to be staged, which changes that figure's subject. Both are reasons the obvious verification keeps being deferred, and both are why this is a memo rather than a task that was simply skipped.

**Do it as part of the next genuine re-capture, not as a test.** The cheapest honest moment is whenever a Windows frame next needs re-shooting for its own sake: the machine is taken anyway, the subject is changing anyway, and the staging is being done anyway. Verifying then costs nothing extra. Running a capture *purely* to test the path spends the user's screen and changes a committed frame to learn something that the next real capture would have told us for free.

**Two things to check when that happens**, neither of which the parse checks can see: that the stroked frame measures 2px of `#BDBDBD` at full alpha on all four sides and composites to 189 on both a white and a dark page (matching the ten frames as they stand), and that `terminal-tabs-windows.png` gets its border on the **crop's** four sides rather than on the pre-crop window.

Related: the Windows capture path also has no `assert_publishable` guard, where the macOS side enforces the publishable-session rule in code (`dashboard.py`). That is its own gap and its own memo — but it bites in the same moment, since both are only testable when a capture actually runs. Raised by the AIR session, which reproduced a failure its own manifest entry had documented two days earlier.
