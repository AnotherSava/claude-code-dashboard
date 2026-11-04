---
created: 2026-11-03 08:45:38
---

# Carry the terminal's own session/pane address from the hook to each row, behind the adapter seam

Proposed by the agterm session (macOS) on 2026-11-03. Today claude_hook.py posts client, event, payload, console_pids and agent_pid. Claude's payload carries no environment, so no terminal address reaches the dashboard. Inside agterm every hook inherits AGTERM_SESSION_ID and AGTERM_PANE_ID, which stay fixed for the pane's life; outside agterm both are absent.

What it would buy:
- session_registry's tab_pid / inbox_for: with two Claude sessions in one folder they refuse to choose. A pane id maps the row to its tab exactly.
- Labelling: the context write is already addressed by `--target`/`--window`, using the key label_targets minted from a `tree` walk. A pane id from the hook could replace the title join that picks which session gets a row's label. The walk would stay, since labels::plan also needs each session's current context and the sessions no row claims.
- Visit detection: agterm's `session.selected` event (already in 0.36) carries the session id, so a departure could be tied to the row through a stored id rather than its title. The departure's `tree` call would stay, because it also supplies the session's occupant, and person_verdict needs that. Relates to the unadopted session.selected / lastSelectedAt / close --undoable items in attention_visit_detection_design.

The agterm session proposed a literal `"agterm": {session, pane}` field stored on the row. That breaks the TerminalAdapter seam rule in CLAUDE.md (no terminal named downstream of the seam). The seam-shaped version: the hook forwards the terminal's address uninterpreted, the row stores it as an opaque key (like FrontReading::surface), and only the agterm adapter reads it. Windows leaves it empty until agwinterm or tmux supply an equivalent.

A second route to the same ids needs no hook change: read them from the environment of the pid the session registry already holds (`ps eww -p <pid>` on macOS). It is the open memo map-a-registry-session-to-its-agterm-tab-by-reading-the.md. The two memos are rival answers to one item; building either one delivers the row-to-tab mapping, so close both together.
