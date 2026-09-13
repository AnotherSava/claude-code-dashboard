---
created: 2026-09-08 04:01:00
---

# Docs site builds from an unpinned remote_theme in docs/_config.yml, so GitHub Pages…

Docs site builds from an unpinned remote_theme in docs/_config.yml, so GitHub Pages serves whatever it last cached and the theme can change under the site with no commit here. Pin it: remote_theme: just-the-docs/just-the-docs@<current release, v0.12.0 as of 2026-09-08, re-resolve when doing it>. The attribution footer is already suppressed here by _includes/nav_footer_custom.html, so the pin is the only thing missing. Detail in the github-pages skill, section Auditing a site that already exists. Found 2026-09-08 auditing six local docs sites.
