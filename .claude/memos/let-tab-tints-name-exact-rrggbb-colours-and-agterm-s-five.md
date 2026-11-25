---
created: 2026-11-24 16:12:36
---

# Let tab_tints name exact #rrggbb colours and agterm's five new named ones

Suggested by the agterm session, 2026-11-24.

Since agterm v0.41.0, session tint, workspace tint and --tint take any #rrggbb as well as the named colours, and the named set grew by five: teal, pink, brown, indigo, mint. A hex-tinted row shows a checked "Custom #rrggbb" entry in its menu. On 2026-11-24 /Applications/agterm.app held this build, but the running instance was 0.40.0 until relaunch.

Here: terminals::TintColour is a closed enum of agterm's original seven, used by config.tab_tints (TabTints, model and remote) and by agterm_wire::tint_of when it reads the tree's tint back. Accepting a hex string beside the names would let one tab_tints value name an exact colour. Check what tint_of does on a tree tint it can't parse (a hand-set teal or #hex) before widening, since tints::plan compares the read tint against the one it wants. The write needs a version gate like TINT_SINCE for anything past the original seven.

agwinterm: its origin/local branch has no per-session tint (whole-word tint only hits the sidebar-tint brightness setting), and the agwinterm adapter builds every LabelTarget with tint: None. The user wants any agwinterm tint work to take #rrggbb from the start, so one tab_tints value means the same colour on both terminals. As of 2026-11-24 nothing in this repo or its memos records agwinterm tint work planned or under way; when it starts, that is work for the agwinterm repo's session.
