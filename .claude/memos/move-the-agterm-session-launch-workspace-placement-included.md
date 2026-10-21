---
created: 2026-10-21 21:20:03
---

# Move the agterm session launch, workspace placement included, behind the TerminalAdapter seam

session_launcher::launch_macos drives agtermctl directly (session new, the unrealized check, close), and since commit 0d87b35 it also picks the workspace through placement_now, which reads agterm's window list and workspace ids, and the cross-platform start functions (start_and_wait, open_and_wait, start_in, launch) carry the agterm-shaped workspace_memory::LastWorkspace. CLAUDE.md's Conventions say anything learned from a terminal belongs on terminals::TerminalAdapter and that code downstream of it names no terminal. Raised by the 2026-10-21 read-only review (r1-2, common, structure only) and deliberately left for its own change, since moving placement alone would be half the job. Next step: add an adapter method that starts a session in a directory (placement and focus decided inside the agterm adapter, workspace_memory owned by it), have session_launcher call it, and leave the Windows launcher (memos about agwinterm session new / the remote-session holder) a place to plug in.
