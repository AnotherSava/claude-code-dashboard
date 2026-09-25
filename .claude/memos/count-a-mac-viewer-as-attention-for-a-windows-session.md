---
created: 2026-09-24 18:45:59
---

# Count a Mac viewer as attention for a Windows session watched through tmux

Windows sessions now run inside the claude repo's WSL tmux holder (remote-session), and a Mac agterm client can watch the same session. The Windows dashboard cannot see that viewer: its AFK check (idle.rs idle_ms) measures Windows desktop input, and attention (attention::observe) learns only from Windows Terminal captions. So a user reading a finished Windows session from the Mac looks away. notifications::fire_reason still sends the done ping, and the row stays unread (🟢) on both tabs until it is looked at on Windows. The Mac dashboard cannot help as things stand: attention::resolve_row considers only rows with origin.is_none(), and AgentSession.attended_at is serde(skip), so nothing a Mac read produces reaches Windows.

Reasoned from code by the 2026-09-24 tmux title audit, not reproduced. Direction is open: some presence or seen signal carried from the Mac viewer to the owning dashboard.

The user asked for ideas on how to handle it (2026-09-24) and parked the question here, so picking this up starts with laying out the options and a recommendation before building anything.
