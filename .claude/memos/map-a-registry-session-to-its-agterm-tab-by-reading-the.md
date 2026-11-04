---
created: 2026-11-03 20:22:05
---

# Map a registry session to its agterm tab by reading the agent's AGTERM_* environment

Suggested by the agterm session, 2026-11-03, from its cookbook recipe session-snapshot (agterm repo, cookbook/session-snapshot/: snapshot.sh, render.py, restore.sh). Not yet checked against this repo's code.

How the recipe maps a Claude conversation to an agterm tab with no hook: walk ~/.claude/sessions/*.json, keep the entries whose pid is alive, and read that process's environment for AGTERM_SESSION_ID, AGTERM_PANE and AGTERM_PANE_ID. Skip an entry that has another registered Claude among its ancestors, so a nested Claude is not taken for the pane's own. The agterm session measured on this Mac that `ps eww -p <pid>` shows AGTERM_SESSION_ID for same-user processes.

Why it matters here: it gives the exact row-to-tab mapping that session_registry's tab_pid and inbox_for refuse to guess when two interactive sessions share a folder (a --fork-session --resume migration). It is also an alternative to the earlier agterm-ids proposal of adding the AGTERM_* ids to the hook body in integrations/claude_hook.py, the open memo carry-the-terminal-s-own-session-pane-address-from-the-hook.md: the dashboard already holds the pid from the registry, so no hook change is needed. The two are rival answers to one item; building either one delivers the mapping, so close both together, carrying this memo's restore comparisons below over if they are still wanted.

Also in the recipe, for comparison with session_restore.rs (title plus pid registry): it resumes by conversation id, so two sessions in one folder and a split's two panes each come back to their own conversation, and restore.sh refuses to resume a conversation a live Claude already holds, so the history does not interleave.

The agterm session's advice is to skip the snapshot HTML page, since session_restore already restores automatically, and the recipe only sees open windows.

macOS only as described (ps eww). Windows would need its own way to read another process's environment.
