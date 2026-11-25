---
created: 2026-11-24 00:35:51
---

# Give a running local session a resume tab when a message reaches it and it has no agterm tab

Suggested by the agterm session (sent 2026-11-22, re-sent 2026-11-24).

When a Claude session in agterm (local, or a remote attach.sh tab) messages or opens another session that has no agterm tab on this Mac, that session should get one.

Already covered: a session the relay or /api/session/open just started (attach_started in http_server.rs, gated on the receipt's hosted_session), and a running Windows cc-* tmux session a message reached (sync::offer_tab, commit eb797c1, logged tab_offer, which asks the peer to open an attach.sh tab through POST /api/sync/attach).

Not covered: a local claude started outside agterm (VS Code terminal, Terminal.app, headless) that a message reaches. It would want a resume tab in agterm. It must not open a second tab where one already exists: pick-session.py in the claude repo matches a tab by attach.sh's argv, and GET /api/session/tabs (agterm_wire::agent_tabs) is the local equivalent.

Open question: a resume tab for a conversation a live claude already holds would interleave history (see the session-snapshot recipe's refusal in map-a-registry-session-to-its-agterm-tab-by-reading-the.md). So this probably means either finding the existing process's terminal, or offering a tab only once that process has exited.

Not a duplicate of give-a-mac-session-started-for-a-windows-agent-a-tab-on.md, which covers the reverse direction.
