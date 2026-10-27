---
created: 2026-10-27 15:29:44
---

# Add features-page figures for unread versus read DONE rows and the instruction-adherence colours

Two figure gaps screenshot-relevance raised on 2026-10-26 for docs/pages/features.md, both stageable on Windows through the existing hero fixture (docs/screenshots/capture/fixtures/hero.json, replayed by lib/dashboard.py fixture-up through /api/event). (1) 'Finished, and not yet read': one unread DONE row (green pill) beside one read DONE row (paler pill), plus a row carrying the xN name-sharing marker. The prose describes the colours and the badge in words only. (2) 'Instruction adherence': the agent name tinted green (Alive), amber (Pending) and red (Dead), and a row with the drift warning badge; the feature is off by default (instruction_canary_enabled), so most readers never see it. Next step: decide whether these are two figures or one composite, extend the fixture with the needed states (read state needs attended_at, which the fixture may need a way to set through a real input such as opening the history window), write capture scripts under docs/screenshots/capture/, add manifest entries without a policy (the user sets it), and embed with figure.html includes.
