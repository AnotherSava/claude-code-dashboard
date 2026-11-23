---
name: tab_tint_ownership
description: The dashboard owns every agent tab's and ⇄ tab's agterm row tint (model over remote), per machine from tab_tints; attach.sh's tint is retired
metadata:
  type: project
---

Decided by the user on 2026-11-23: this dashboard owns the sidebar-row tint on every agent tab and every `⇄` tab on the
machine it runs on, from that machine's own `config.tab_tints`. A session off the `ANTHROPIC_MODEL` default takes the
model tint, which wins over the remote tint; a `⇄` tab otherwise takes the remote tint (default gray on icon and
name, the colour the claude repo's `attach.sh` used). Different colours per machine are an accepted side effect, not a
goal. The dashboard reads no config of the claude repo for this.

The claude repo's `attach.sh` tint block and its `REMOTE_TINT_*` keys are retired by that repo's session once the Mac
dashboard is deployed with the tint; until then both write the same gray. The `⇄` title badge stays owned by the
claude repo's `lib.sh`/`attach.sh` for now; moving it here is a separate open idea in the memo backlog.

**Why:** the Mac dashboard is the only process on the Mac that learns a Windows-hosted session's model (over sync),
and one owner keeps the precedence rule in one place.

**How to apply:** a request to colour or format a tab goes through `terminals::tints` and `config.tab_tints`, never
back into `attach.sh`. Related: [[agterm_status_coexistence]].
