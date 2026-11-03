---
created: 2026-11-03 10:25:33
---

# Show an agent's dashboard history in the right pane of a split on its agterm session

Moved from the agterm repo's backlog by the agterm session on 2026-11-03; all of the work is dashboard-side. Body as it was sent:

Show the dashboard's history for an agent as a page in the right pane of a split on that agent's agterm session, so the agent stays visible on the left.

    agtermctl session split on --target "$AGT_SESSION_ID"
    agtermctl session overlay open --url "http://127.0.0.1:<dashboard-port>/<history-route>" --chromeless --persistent --pane right --target "$AGT_SESSION_ID"

- Trigger: agterm's title-bar context text has no click handler, so clicking it needs an upstream agterm change. Until then, use a `command` line in ~/.config/agterm/keymap.conf with a chord, passing `{AGT_SESSION_ID}`, or a button in the dashboard UI.
- Depends on the agterm-ids proposal sent to this session on 2026-11-03 (`AGTERM_SESSION_ID` in each hook event body), so the dashboard can map an agterm session to its row. That proposal is the open memo "Carry the terminal's own session/pane address from the hook to each row, behind the adapter seam".
- `--chromeless` is allowed only for a loopback host, which the dashboard's server is.
- Closing the page leaves a plain shell in the right pane, so the command should also close the split (`agtermctl session split close`), or the toggle should close both together.
- A pane overlay is refused when the right pane already holds one (`pane overlay already open`), and when the split is hidden (`pane not visible`).
- Rejected: an agterm companion session. It is a separate tab rather than side by side, and it makes the agent's shell respawn instead of closing when Claude exits. The window walk, restore and idle close would also all have to skip it.
- Unverified: neither command has been run, and the history route does not exist yet.
