---
created: 2026-09-30 14:46:52
---

# Carry the initiating agent's current task on the relay envelope, and store it on the row

User's idea, 2026-09-30, refined in the same conversation. Closes the gap that [[a-subagent-permission-prompt-can-invent-a-row-that-pings-you]] and [[a-relayed-peer-message-becomes-the-receiving-row-s-label-a]] each hit from one side: one row has no text to show, the other has the whole relay envelope as its text. Both are "an agent initiated this work, so use what that agent supplied" — and for the relay case the supplying is already half-built.

WHAT EXISTS. `MessageEnvelope.from_label` (`sync.rs`) is plumbed end to end: `POST /api/message` accepts it (`http_server.rs`, the `from_label` field on the request), it rides `send_message_hop`, and `skills/shared/peer_relay.py` has a `--label` flag that sends it.

WHAT IS MISSING, and it is the whole point. Its only consumer is `peer_message::build_content`, which interpolates it into the message body as `Sender's own description: {l}`. Nothing stores it. So the receiving dashboard, which *built* that string, can only get the fact back by parsing its own prose out again — which is exactly the parse-and-guess this avoids. Two lesser problems on top: the `peer` skill documents `--label` as "<who you are>", so what it carries today is identity rather than the current task, and it is optional, so most sends omit it.

THE CHANGE, three touch points:

1. **The sending dashboard fills it, not the calling agent.** `http_server::post_message` already resolves the sender's row (it keys the `peer_send` log line by the sender's chat_id), so it holds that row's `original_prompt`. Stamp it there. This is deliberately NOT "the agent passes its prompt": an agent self-reporting what it is working on is a claim, and a dashboard reading its own row is an observation, and the two come apart precisely when the agent is confused about its own state. The precedent is in the same struct — `reply_to` is minted by the dashboard rather than taken from the caller, documented as "the only place holding both halves *exactly*", for this reason.

2. **The receiver stores it on the row** rather than only rendering it. That is what lets the row's label and any notification say what the initiating agent was doing, with no parsing.

3. **Keep rendering it in the body too.** The receiving model still benefits from reading it, and `header_safe` already redacts it against the trust vocabulary. Storing it is an addition, not a move.

DECIDE WHILE BUILDING: whether a caller-supplied `from_label` still wins over the dashboard-stamped one (an agent may have a better description of *why* it is messaging than its own prompt gives), or whether the two become separate fields — the caller's "why I am writing" against the dashboard's "what I am doing". The second is more honest and is one more field; the first is cheaper and has to pick a precedence. Not settled here.

OUT OF SCOPE, and worth saying because it looks adjacent: this does nothing for the subagent-invented row, whose initiator is a subagent on this machine rather than a peer, and which has no envelope at all. That row's text has to come from `agent-<agent_id>.jsonl`, which is the other memo's third fix.
