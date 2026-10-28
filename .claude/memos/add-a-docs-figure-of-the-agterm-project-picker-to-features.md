---
created: 2026-10-27 16:30:07
platform: macos
---

# Add a docs figure of the agterm project picker to features.md

The "Open a project from agterm" section of docs/pages/features.md describes the picker (integrations/pick-local-session.py, bound in agterm) and its "running" mark in words only; docs/screenshots/ has no figure of it. screenshot-relevance raised the gap on 2026-10-26 (routed from the Windows session, which cannot capture it).

Steps, per the screenshot skill: write docs/screenshots/capture/<id>.sh using DocShot for the shutter, staged so the list holds only publishable project names with one shown as running; add a manifest entry with no policy (the user sets it); embed it with a figure.html include in features.md.

The capture opens agterm's picker and takes the Mac's screen, so ask the user right before running it.
