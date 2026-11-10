---
created: 2026-11-09 18:29:11
platform: windows
---

# Confirm the first agwinterm idle close after 0.20.13.2 logs closed and the tab goes

The idle close now reads agwinterm's exitedPanes tree field (agwinterm fork 0.20.13.2) to tell that a wsl tmux attach tab has exited after /exit, then sends session.close. Before it, nine closes on 2026-11-09 logged idle_close close_failed 'still not back at a shell after 30s' and left [exited] tabs. On 2026-11-09 agwinterm here was still 0.20.13.1 (agwintermctl ping), so the fix never ran live. Next step: once agwintermctl ping reports 0.20.13.2 or later and the dashboard is deployed, wait for an idle_close in widget.jsonl and check its outcome is closed and the agwinterm tab is gone; a close_failed reading 'neither fallen back to a shell nor exited' means exitedPanes is not reaching agwinterm_wire::agent_gone. Known unfixed: an agent in pane 1 of a split is removed rather than listed as exited, so that close still times out (review finding r1-1).
