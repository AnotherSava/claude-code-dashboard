---
name: verify_peer_message_delivery
description: peer_write logs the receiving pid, not the agent name; map it through Claude Code's session registry to learn which agent actually got a relayed message
metadata:
  type: project
---

The cross-machine relay's receipt reports `written`, which by design means only that a listener accepted the bytes — it cannot say delivered. The one thing that *is* recoverable is **which session** received them, and it takes two steps because `peer_write` records a bare pid:

```bash
grep -o '"decision":"peer_[a-z]*"[^}]*' "$APPDATA/com.anothersava.claude-code-dashboard/widget.jsonl"
# then, per pid, read Claude Code's own session record:
python -c "import json;d=json.load(open('$HOME/.claude/sessions/<pid>.json'));print(d['cwd'],d['name'],d['status'])"
```

**Why:** Reading the log alone answers "a write succeeded" but not "to whom" — and a dashboard row is cwd-derived, so the pid is the only link back to a project. On 2026-08-30 this turned two look-alike `outcome = "written"` lines into two *different* targets: one to this repo's session and one, eleven minutes later, to an idle `transcripts` session. Without the lookup they read as one message plus a retry.

**How to apply:** Use it whenever a relayed message's fate is in question — a peer reporting `written` with no visible effect usually means it reached a different session than intended, not that delivery failed. `messagingSocketPath` in the same record is what the relay actually wrote to. Registry records only exist for sessions Claude Code tracks, so a headless `claude -p` target yields nothing and the absence proves neither delivery nor failure. See [[debug_state_transitions_via_widget_jsonl]] for the wider decision-log grep, and the `investigate` skill for state reconstruction.
