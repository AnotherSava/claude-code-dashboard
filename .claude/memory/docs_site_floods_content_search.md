---
name: docs_site_floods_content_search
description: Exclude docs/_site from every grep/glob over docs/ — the gitignored Jekyll build mirrors each page as one 30KB line
metadata:
  type: project
---

Scope any content search over `docs/` to exclude `docs/_site/`. It is the local Jekyll build (3.1 MB, gitignored at `.gitignore`'s `/docs/_site/`), and it holds a rendered copy of every page — so a grep that matches a phrase in `docs/index.md` matches it a second time in `docs/_site/index.html`, where the theme's compressing layout has collapsed the entire page onto **one line**. Ripgrep prints that whole line, so a single incidental hit dumps ~30 KB of markup into the reply. Measured 2026-09-14 searching the docs for a screenshot placeholder string.

**How to apply:** add `--glob '!docs/_site/**'` (or `':!docs/_site'` for `git grep`) whenever the search path is `docs/` or the repo root, and prefer `--include=*.md` when only the sources matter. The same shape applies to `docs/vendor/` and `docs/.jekyll-cache/`, ignored on the lines beside it.

The build is also **stale by construction** — nothing regenerates it on a pull — so its HTML is evidence about whenever it was last built and never about the current sources. It showed the pre-`hero-macos` "screenshot hasn't been taken yet" note months after that frame existed. Read `docs/*.md` and `docs/_includes/` for what the site says now.

Related: the global rule excluding `node_modules/` from all searches, in `~/.claude/CLAUDE.md`.
