---
created: 2026-11-03 11:20:03
---

# Repoint check-figures.py's manifest-reference comment at the docs-screenshot skill

A comment in docs/screenshots/check-figures.py (the verifiedAt handling) says "The skill's references/screenshot-manifest.md defines" the schema. The screenshot skill was split on 2026-11-03: the documentation process, contact-sheet.py and references/ now live in the docs-screenshot skill (~/.claude/skills/docs-screenshot/references/screenshot-manifest.md), while the shutter and framing scripts stay under ~/.claude/skills/screenshot/scripts/. Name docs-screenshot in that comment. Comment-only; every other mention of "the screenshot skill" in this repo names a script that did not move.
