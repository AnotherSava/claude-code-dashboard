---
name: context-window-map-needs-generation-updates
description: context_window_tokens default map needs manual review per new Claude model generation, not just per family
metadata:
  type: project
---

`src-tauri/src/config.rs`'s `context_window_tokens` default map needs a manual
review whenever Anthropic ships a model whose default context window differs
from its family's existing entry — prefix-matching (`notifications::window_for`)
doesn't infer this automatically.

**Why:** Sonnet 5 shipped with a 1M default context window (previously only
Opus had that), and the map still only had `claude-opus` / `claude` (200k
fallback), so a real session's 541k-token usage computed as 271%. See
[[claude-model-context-windows]] (global learning) for the generation-by-
generation window sizes.

**How to apply:** When a new Claude model family or generation launches,
check its actual context window before assuming the existing `claude-opus` /
`claude` entries cover it. Add a specific key (like `claude-sonnet-5`,
`claude-fable`) rather than broadening an existing prefix (e.g. `claude-sonnet`)
— a broadened prefix would wrongly also catch older sub-versions in the same
family that are still on a smaller default. Extend the regression test in
`notifications.rs` (`default_window_map_reflects_1m_default_on_the_claude_5_family_only`)
alongside any map update.
