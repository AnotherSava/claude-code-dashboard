---
created: 2026-09-20 17:04:00
platform: macos
---

# Re-shoot terminal-tabs-macos: the committed frame predates its own publishable guard

Noticed 2026-09-20 while auditing the backlog. Narrow residue, and most of the surrounding facts are already written down — read terminal-tabs-macos.py's module docstring and the PUBLISHABLE_PROJECTS comment in capture/lib/dashboard.py before doing anything here, because both answer questions this memo would otherwise re-open.

THE STATE: the committed terminal-tabs-macos.png was taken by hand before the capture script existed, so it passed none of the script's gates. Its workspace headers and session rows carry names that PUBLISHABLE_PROJECTS does not vouch for — the script's own docstring says so in as many words. The frame is tracked in a public repo, so those names are published. Nothing is misconfigured: the guard works, and it would refuse this frame today.

WHAT IS *NOT* WRONG, so it does not get 'fixed': what-is-next is on PUBLISHABLE_PROJECTS on purpose. It is private, and it is listed because its owner cleared it on 2026-09-11 for these frames, knowing they carry its prompt text and not only its name. The list is named PUBLISHABLE rather than PUBLIC to hold exactly that distinction. Removing it would be wrong and would break compact-mode-macos, whose only flagged name is that cleared one. An audit of this frame on 2026-09-20 initially called that entry an error, purely from grepping the constant and never reading the comment above it.

THE WORK: re-shoot terminal-tabs-macos with a sidebar whose every visible name is either on the default list or named explicitly on --publishable, the latter only for repos that really are public or whose owner has cleared them the way what-is-next's did. The script refuses the capture otherwise, which is the point. Keep the subject: same sidebar composition, and the spread of glyphs and the two [N%] rows that screenshots.json and features.md alt text both promise, since the percentage comes from the transcript watcher and cannot be faked.

The names themselves are deliberately not listed here — this file is committed to the same public repo as the frame. Read them off the image, or run the capture script and let it name them in its refusal.

Related: [[rework-the-docs-screenshots-the-windows-half-is-done-the]] carries the other outstanding macOS re-shoots.
