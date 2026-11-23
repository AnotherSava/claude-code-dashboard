---
created: 2026-11-23 16:12:57
---

# Let the Mac dashboard write the whole ⇄ tab title, badge included

Today the claude repo's remote-session/lib.sh adds the ⇄ badge through tmux set-titles-string for clients attached from another machine, attach.sh writes the connecting and failed titles, and this dashboard only parses the badge (terminal_title::REMOTE_BADGE, which must change with lib.sh's REMOTE_SESSION_BADGE). The idea: the Mac dashboard writes the whole ⇄ tab title from the synced row (glyph, badge, context %), as it already owns the tab's tint and context line, and lib.sh stops forwarding titles to Mac clients. Gains: one owner for everything on a tab, and the cross-repo lockstep goes away. Costs: the glyph on a ⇄ tab then depends on sync, where today it stays live through tmux while sync is down; attach.sh still writes the connecting/failed titles, since only it knows the ssh state. Needs a decision with the claude repo's session, which owns lib.sh. Deferred by the user on 2026-11-23 while building the model tint.
