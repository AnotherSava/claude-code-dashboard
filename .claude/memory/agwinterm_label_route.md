---
name: agwinterm_label_route
description: Why agwinterm shows status via the OSC title and the prompt via session context; rename, caption and capabilities routes were built and removed on 2026-10-02
metadata:
  type: project
---

**Status reaches agwinterm's sidebar through the window title, and the prompt goes in `session context`.** The dashboard writes only the context line. It does not rename sessions or use a fork verb for the prompt.

The routes built on 2026-10-02 and then deleted, so they are not rebuilt:

- **Renaming sessions** (`<glyph> <project>` via `session.rename`, `[N%]` in context). This brought in ownership rules, cleanup at startup, glyphs saved into agwinterm's state and overwriting of user renames. Meanwhile the console-title write was already reaching agwinterm through tmux `set-titles`. Two reviews judged it the expensive route.
- **`session.caption`**, a title-bar-only fork verb. agterm has no such verb and puts a session's prompt in `session context`, so the caption had little upstream chance. The user chose agterm parity so that the fork can eventually be retired.
- **`capabilities`**, a verb listing optional verbs. agwinterm's `unknown command '<cmd>'` reply already identifies a missing verb, and the hand-kept list went stale at once.

**Fork dependency.** The route needs the fork's tree `title` (sessions are matched to rows by title, and read/unread departures are named from it) and its title-following sidebar. On a stock agwinterm the context line still works, but the sidebar shows `session N`. On 2026-10-02 those fork commits were on agwinterm's `local` branch, unpushed.

**How to apply:** check this list before proposing a new agwinterm field. The bar is agterm parity, or a generic change that upstream (yeroo/agwinterm) would take. Mechanics are in the global `agwinterm.md` learning.
