---
created: 2026-11-03 20:18:24
---

# Put more than the dev-server port into each agent's agterm info line

Requested from the agterm session (the user asked for this memo), 2026-11-03.

agterm 0.40 gives each session one `session info` line under its sidebar row. The dev-server work (dev_servers.rs, written through TerminalAdapter::show_servers) already fills it with a green `▶ dev server :PORT` (`dev servers :P1 :P2` for several; `agterm_wire::servers_line`), and the dashboard is its only writer. The line is shared: a second writer overwrites the first and the last set wins.

Upstream's cookbook recipe `claude-git-info` (agterm repo, cookbook/claude-git-info/) fills the same line from a Claude Code hook with the repo's git state, so installing it as-is would clobber the port. Instead, have the dashboard compose one line itself from several facts.

The recipe's git forms, which the user called out:
- `🌿 master ✓ clean`: on a branch with an upstream, nothing changed.
- `🌿 feature ● 3 ↑2`: three changed or untracked files, two commits the upstream lacks.
- `🌿 master ● 1 ↑1 ↓4`: the upstream also has four commits this branch lacks.
- `🌱 spike ● 1 local only`: no upstream configured.
- `📍 b64de22 detached ✓ clean`: detached HEAD.

Styling in the recipe: branch bold, `✓ clean` green, the count orange, `↑` blue, `↓` purple, `local only` dim, all via `session info --markdown` with `^[text](color: "…")` spans. The line has a 512-byte cap on the source (markup included) and must stay one line.

Combined example: `🌿 feature ● 3 ↑2 · ▶ dev server :5173` — answers "which agent left uncommitted work, and where's its server" from the sidebar.

Timing: the recipe refreshes on start, after every edit or shell command, and at turn end; worth borrowing over polling (the hook stream already reaches the dashboard). Note the 45s self-expiry the dev-server line uses today.
