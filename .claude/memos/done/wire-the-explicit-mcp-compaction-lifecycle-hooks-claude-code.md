---
created: 2026-06-27 16:16:00
---

# Wire the explicit MCP/compaction lifecycle hooks (Claude Code 2.1.x): ElicitationResult…

Wire the explicit MCP/compaction lifecycle hooks (Claude Code 2.1.x): ElicitationResult as a clean unblock signal (user answered the MCP prompt -> leave Blocked) and PostCompact as a cleaner 'compaction done' than inferring it (PreCompact already drives the boundary). Low effort, clarity-only — not structural. From the hooks/state-logic review (Tier 2). [DONE: wired ElicitationResult → resume Working (adapter + setup snippet + hook + CLAUDE.md). Skipped PostCompact — read-only, and we infer no 'compaction done' anywhere (PreCompact already marks the boundary), so it'd be a no-op Ignore.]
