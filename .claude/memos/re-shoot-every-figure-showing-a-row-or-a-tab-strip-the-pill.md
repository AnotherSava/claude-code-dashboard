---
created: 2026-09-30 13:51:17
---

# Re-shoot every figure showing a row or a tab strip: the pill and glyph vocabulary changed underneath them

Shipped 2026-09-30 in the DONE/IDLE redefinition. Six committed frames now show a state vocabulary the docs no longer describe. None of the existing per-figure memos mentions this reason, so whoever picks one of those up will re-shoot for its own reason and not know to check the states in frame.

WHAT CHANGED, and it is two separate things in the same change set:

1. The PILL. A finished row the user has read used to render the IDLE pill (#2f2f33 background, #a1a1aa text) with the word IDLE. It now renders DONE in the inverse of that pill (#a1a1aa background, #2f2f33 text). So the WORD on those rows changed, not only the colour — a frame showing a read row says IDLE where the running app now says DONE. IDLE still exists and still renders the dark pill, but it now means CLEAN (nothing to come back to), which is a state none of the committed frames contains.

2. The TAB GLYPH. `terminal_title::status_glyph` is now a function of (status, read) rather than status alone. 🟢 is finished-and-unread and ⚪ is finished-and-read, as before — but ⚫ is new and means CLEAN, and no committed frame has one. `docs/pages/features.md` and `docs/pages/settings.md` both now list seven readings where the figures show four.

THE FRAMES, from screenshots.json's own `shows` text:
- hero-windows, hero-macos — both describe "idle" rows among seven. Under the new vocabulary those rows are either DONE (if they were finished-and-read) or genuinely CLEAN (if cleared), and the two now look different from each other and from what is in the picture.
- compact-mode-macos — its `shows` says "IDLE/WORK/DONE/WAIT badges" explicitly.
- compact-mode-windows — same subject, rows reduced to name and state badge.
- terminal-tabs-macos, terminal-tabs-windows — the glyph strip, missing ⚫ and with ⚪ now carrying a narrower meaning.

ONE ALT TEXT IS ALREADY WRITTEN FOR PIXELS THAT DO NOT EXIST: features.md:47's `alt_macos` says "white for one already read", which is true of the glyph's current meaning but describes a frame shot when ⚪ meant idle. That line is not wrong to a reader — a white dot does now mean already-read — but it is a claim about a session state the capture did not have.

ALSO STALE, and the one that will break a capture rather than merely misdescribe it: `docs/screenshots/capture/fixtures/hero.json` was edited in the same change set. The crop-stage row's sequence is now SessionEnd{reason:"clear"} + SessionStart{source:"clear"} so it produces a genuinely CLEAN row, and its `input_tokens` was dropped because a cleared session holds no context. So the committed hero-*.png shows a token count the fixture no longer produces. `lib/dashboard.py`'s `check_shown` compares against /api/agents, which is `resolved_snapshot` — raw status, no read flag, no clean-hiding — so it will pass either way; only the picture disagrees.

WHERE THIS SITS RELATIVE TO THE EXISTING MEMOS: the umbrella [[rework-the-docs-screenshots-the-windows-half-is-done-the]] says in as many words that nothing but its corner-ring item belongs in it and that each figure owed has a memo of its own, so this is not an append to that one. The per-figure memos it points at — [[re-shoot-terminal-tabs-macos-the-committed-frame-predates]] and [[port-the-hero-fixture-replay-to-macos-so-hero-macos-is-shot]] — each carry a different reason for the same re-shoot, and doing either without reading this will produce a frame that is correct for that memo's reason and still stale for this one. The Windows half is [[the-windows-capture-scripts-have-never-been-run-end-to-end]].

NOT TAGGED FOR A PLATFORM: half the frames are Windows and half macOS, so this needs both boxes and neither tag would be true.
