---
created: 2026-11-09 12:34:50
---

# Let the commit gate tell the user's own ~/.claude/skills from a contributor's that lacks the screenshot skill

commit-checks.sh marks the ~/.claude-dependent checks NOT COVERED only when ~/.claude/conventions or ~/.claude/skills is absent (6ef7fc1). A contributor who has a ~/.claude/skills folder of their own, but not the user's screenshot skill, still fails the skill-script and pwsh capture checks instead of seeing NOT COVERED. Fixing it needs a positive marker of the user's own setup, e.g. probing for the specific skill files those checks read (docs/screenshots/check-skill-scripts.py names them) and reporting NOT COVERED per missing file rather than per directory. Raised in the 2026-11-08 unattended backlog run's report and never answered; decide whether a contributor without that skill is a case worth supporting.
