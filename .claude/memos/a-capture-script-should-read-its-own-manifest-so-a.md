---
created: 2026-09-13 08:11:47
---

# A capture script should read its own manifest, so a documented failure can refuse a shot

Idea from the tauri-dashboard session on CHROME, 2026-09-13, and it is the best thing to come out of that exchange.

THE EVIDENCE IS A FAILURE THAT WAS ALREADY WRITTEN DOWN. `hero-macos`'s manifest entry in `docs/screenshots/screenshots.json` carried this step, from a probe on 2026-09-11:

    "A ROW RESTORED AFTER A RESTART HAS NO LABEL OF ITS OWN, and the widget then falls back to
     the last entry in its stored dialog. That is how a relayed agent-to-agent message reaches
     the picture verbatim ... The fix is a fresh typed prompt in that session"

On 2026-09-13 that exact failure was reproduced — a hero shot went out with a row reading `<cross-session-message from="uds:/tmp/cc-socks/33269.sock" from-na...` — and was reverted. Three dashboard restarts were spent around it. The information existed, in the right file, keyed to the right figure, two days early. Nothing could act on it because nobody read it.

THE GENERAL SHAPE, which is what makes this worth building rather than just being more careful: `capture.steps` is PROSE FOR A HUMAN and the guards are CODE, they encode overlapping knowledge, and nothing forces them together. So they drift in the one direction that is silent — the manifest learns something the script cannot enforce. `hero-macos.py` already refuses on a name that is not in `PUBLISHABLE_PROJECTS`, on a missing blocked/working/done spread, on a stale usage cache and on synced rows; it does not refuse on a row whose label is a transport envelope, even though its own entry says that is a known way to ruin this frame.

WHAT IT MIGHT LOOK LIKE — not designed, just the shape. A machine-readable sibling to `steps`, e.g. `capture.refuse`, holding predicates the script evaluates against the roster and the stored prompts before the shutter: a row label matching a transport-envelope pattern, a label over N characters, a name off the publishable list. `steps` stays prose for everything that cannot be expressed that way. The test of the design is whether the 2026-09-11 note could have been written as a refusal rather than a paragraph.

THE SIBLING GAP, from the same exchange and the reason this is not just a macOS chore: the WINDOWS capture scripts have no publishable-names guard at all. `assert_publishable` exists only in `capture/lib/dashboard.py`; `lib/dashboard.ps1` has no counterpart, so on that machine the rule "every session in frame is a publishable repo" is enforced by whoever is looking at the tab strip. That is already memoed separately as the capture-parity item in the `claude` repo, and this is its other half: one side is code missing a check, the other is code that had a check plus a manifest that knew better. Neither machine can see the other's blind spot, which is the same argument one level up.

TWO THINGS NOT TO DO. Do not let a refusal predicate become a reason to edit a stored dialog to tidy a screenshot — the manifest already forbids that, and the fix for an envelope label is a fresh typed prompt in that session. And do not build this while the screenshot work is mid-flight; it is a design change to the manifest schema and both capture libraries, and it earns its place only if a second instance of "the manifest knew" shows up, or if the parity work is being done anyway and this rides along.
