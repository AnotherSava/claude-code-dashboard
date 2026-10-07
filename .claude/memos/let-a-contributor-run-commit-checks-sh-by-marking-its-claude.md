---
created: 2026-10-07 02:18:11
---

# Let a contributor run commit-checks.sh by marking its ~/.claude steps NOT COVERED

The gate's conventions step is an unconditional `python ~/.claude/conventions/check.py .`, and a later step shells out to a script under `~/.claude/skills/docs-relevance/scripts/`. Neither path exists in a clone made by someone who does not have the dotfiles, so an outside contributor cannot run the gate this repo tells them to run before committing — they get an interpreter error on the first step rather than a result.

The gate already holds the right idiom: it prints a NOT COVERED line naming what went unchecked when a step cannot apply on this platform (the Windows capture-script check on darwin). Apply the same treatment to the `~/.claude`-dependent steps — test for the file, run it when present, print one NOT COVERED line naming what was not checked when absent. Never skip silently: a gate that cannot tell 'did not run' from 'passed' is exactly what that idiom exists to prevent.

Found 2026-10-06 while adding the registry-absent fallback to scripts/dev.mjs, which exists for this same audience: `npm run tauri dev` now works in a bare clone while the commit gate still does not. Deliberately kept out of that change set, since folding it in would have made the commit message describe work it did not contain.
