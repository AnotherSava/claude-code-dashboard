---
created: 2026-09-08 04:01:00
---

# Docs site builds from an unpinned remote_theme in docs/_config.yml, so GitHub Pages…

Docs site builds from an unpinned remote_theme in docs/_config.yml, so GitHub Pages serves whatever it last cached and the theme can change under the site with no commit here. Pin it: remote_theme: just-the-docs/just-the-docs@<current release, v0.12.0 as of 2026-09-08, re-resolve when doing it>. The attribution footer is already suppressed here by _includes/nav_footer_custom.html, so the pin is the only thing missing. Detail in the github-pages skill, section Auditing a site that already exists. Found 2026-09-08 auditing six local docs sites.

[DONE 2026-09-18, as convention version 007-docs-theme-pinned rather than as its own change:
`docs/_config.yml` now reads `remote_theme: just-the-docs/just-the-docs@v0.12.0`, the tag this
memo named, with a note above it saying what the missing ref did. The rest of the shape was
already there — `jekyll-remote-theme` under `plugins`, `docs/index.md` and `docs/pages/` both
present — so the pin was the only thing missing, exactly as written here. The `docs-theme-pinned`
rule now re-measures it on every commit.]
