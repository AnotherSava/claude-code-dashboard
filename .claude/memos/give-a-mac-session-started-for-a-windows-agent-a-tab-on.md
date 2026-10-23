---
created: 2026-10-23 22:27:17
---

# Give a Mac session started for a Windows agent a tab on Windows, once Mac sessions can be attached from there

When an agent on Windows messages an idle Mac project, the Mac dashboard starts the session in agterm on the Mac, and no tab opens on Windows, where that agent's user is. The other three cases open a tab on the sender's machine through TerminalAdapter::attach_hosted, keyed by the receipt's hosted_session. This one cannot: Mac sessions run in agterm, not in a tmux server another machine can attach to, and the claude repo's remote-session setup only lets the Mac attach to the Windows holder. Deferred by the user on 2026-10-23 ('Skip it for now'). Next step, when it is wanted: decide how a Windows client reaches a Mac session (Mac sessions hosted in tmux, or an agterm-level remote attach), then have the Mac's agterm launch fill Launched::hosted_name and agwinterm implement attach_hosted for it.
