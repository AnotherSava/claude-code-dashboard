---
name: verify_peer_message_delivery
description: a relayed message's real fate lives in the RECEIVING dashboard's log — peer_write logs a bare pid to map through the session registry, and peer_refused carries the true refusal reason
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

**A refused relay is diagnosed on the receiver, not the sender.** The receiving dashboard logs `decision = "peer_refused"` with the real `reason` (`no_such_session`, `ambiguous_target`, `device_mismatch`, …) keyed by the target chat_id — that line is authoritative, and reaching it on the Windows box is just `ssh OlegS@chrome` (see [[machines-private]]). The sender's `reason` was **not** trustworthy before 2026-08-31: `send_message_hop` judged a non-2xx by status code before reading the body, so a peer's `404 no_such_session` came back as `peer_lacks_route` and sent the operator hunting a version skew that did not exist. Fixed by parsing the receipt body first, but the receiver's log stays the tiebreak whenever the two disagree — a peer running an older build is exactly when they will.

**Sending one by hand needs *no* `Origin` header at all** — `origin_blocked` refuses every value except the literal `null`, so adding a loopback origin to look correct earns a `403 csrf`; `/api/message` additionally requires a loopback `Host`. Two agents tripped on this independently on 2026-09-02. Details in [[debug_synthetic_hook_events]].

**How to apply:** Use it whenever a relayed message's fate is in question — a peer reporting `written` with no visible effect usually means it reached a different session than intended, not that delivery failed. `messagingSocketPath` in the same record is what the relay actually wrote to. Registry records only exist for sessions Claude Code tracks, so a headless `claude -p` target yields nothing and the absence proves neither delivery nor failure. See [[debug_state_transitions_via_widget_jsonl]] for the wider decision-log grep, and the `investigate` skill for state reconstruction.
