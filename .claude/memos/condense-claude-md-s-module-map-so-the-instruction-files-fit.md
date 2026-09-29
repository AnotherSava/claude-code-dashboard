---
created: 2026-09-28 21:37:26
---

# Condense CLAUDE.md's module map so the instruction files fit Claude Code's 150k budget

Every session starts with Claude Code's warning that the instruction files add up to about 224k chars, over the 150k total limit: the project CLAUDE.md is 146.8k and the global one 77.1k (measured 2026-09-28). The warning is informational only — nothing is truncated (see the global learning claude-code-instruction-file-size.md) — but the whole of both files is spent as context on every turn. The project file alone is within about 3k of the 150k per-file limit, past which a second, per-file warning fires. Nearly all of it is the one-line 'Key module map' paragraph (~140k chars on line 15), which has grown into per-module design history: rejected designs, measured incidents, dated verification notes. Next step: decide the budget (e.g. get the project file well under 75k so the pair fits 150k), then move the long-form rationale for each module into docs/pages/development/ pages or learnings, and leave a map entry per module saying what it owns and pointing at where the reasoning lives. Mind [[claude_md_single_line_merge]]: the paragraph is one line, so do this with no parallel edits pending from the other machine, and consider splitting it into one line per module, which would also end the cross-machine merge conflicts.
