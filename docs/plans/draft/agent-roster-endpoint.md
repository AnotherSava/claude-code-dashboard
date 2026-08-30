# Agent roster endpoint (`GET /api/agents`)

Built 2026-08-30. This records why the route exists, what was deliberately left out, and the defects the
review caught — so the rejected options are not re-proposed and the accepted gaps are not mistaken for
oversights.

## What it is

One read-only route on the existing agent-facing server at `127.0.0.1:9077`:

```
GET /api/agents
```

It returns every session this dashboard tracks — running on this machine and synced from peer machines —
so an agent can answer *"does a session for project X exist, on which machine, in what state, and how
fresh is that answer?"* without SSHing to the other box to find out.

## Why it exists

Claude Code ships cross-session messaging, and its `ListAgents` tool lists peers authoritatively — but
**only on the local machine**. Reaching a session on another machine needs Remote Control, which routes
through Anthropic's servers and is unavailable here (see `peer-message-broker.md`). So an agent on the
Mac has no way to learn whether a counterpart session is live on the Windows box.

This dashboard already knows. It merges local rows with rows synced from peers, and `resolved_snapshot`
is already the single merge point. The roster is that existing knowledge, serialized.

The trigger was concrete: a session needed facts from its counterpart on the other machine, could not
tell whether that session was even running, and fell back to reading its transcript over SSH. The roster
answers the "is it there, and where" half before anything is spent.

Identity works out without new machinery. Remote ids are namespaced `{device}/{raw_id}`, and
`derive_chat_id` normalizes backslashes and strips the configured projects root — so the same project
yields the same raw id on both platforms. `transcripts` and `chrome/transcripts` is the whole answer.

## The design rule: report facts and their age, never a verdict

Remote rows live under a 90s TTL, so a remote status can be up to ~90s stale and a dead device lingers
that long. Every remote row therefore carries `last_seen_age_ms`, and a row whose device is missing from
the freshness map is **dropped rather than reported** — a row with no freshness number is precisely what
this route exists never to emit.

Everything is an age in milliseconds, never an absolute timestamp, so a caller needs no clock agreement
between machines. One caveat is recorded in the code: a remote row's `status_age_ms` derives from the
*sender's* clock and so carries any skew between the machines; it is clamped at 0 so skew can never
produce a negative age, and `last_seen_age_ms` — measured entirely on the receiver's clock — bounds how
much of it to trust.

## Rejected, with reasons

- **A `deliverable` / `sendable` boolean.** A green light computed from data that may be 90 seconds old
  over-claims, and the project's own rule is that an indicator must not assert more certainty than its
  evidence. The caller gets the state and its age and makes its own call.
- **User presence (`idle_ms`, "is anyone watching that machine").** Proposed, then dropped after
  examination: a peer message starts a turn in the receiving *agent* whether or not a human is watching,
  and the reply waits in its transcript. Presence changes no decision, so the field would be data added
  on speculation. What actually matters is whether the session exists, whether it is `Blocked` (parked on
  its user, where a message piles onto a dialog), and how stale the answer is.
- **Syncing the pid to make the row directly addressable.** A pid alone does not get you the target's
  `messagingSocketPath` — that still has to be read on the machine itself, so the trip is unavoidable and
  the field would save one lookup.
- **A message broker.** Out of scope by construction. No queue, no tickets, no rate limits, no sending,
  no mutation. Claude Code already does delivery, framing and loop-breaking; see `peer-message-broker.md`
  for what remains there.
- **A `decision` log line per request.** Every `decision` tag marks a state change and the `investigate`
  skill replays them to reconstruct a row. A polling reader that mutates nothing would bury real
  decisions under entries explaining no state.

## Shape

```json
{
  "device": "air",
  "sync_listening": true,
  "peers": [ { "device": "chrome", "last_seen_age_ms": 4120, "sessions": 2 } ],
  "agents": [
    { "id": "tauri dashboard", "project": "tauri dashboard", "device": "air", "local": true,
      "status": "working", "label": "add the /api/agents route", "status_age_ms": 18400 },
    { "id": "chrome/transcripts", "project": "transcripts", "device": "chrome", "local": false,
      "status": "blocked", "label": "Should I drop the legacy column?",
      "status_age_ms": 236500, "last_seen_age_ms": 4120 }
  ]
}
```

`project` is the de-namespaced id — the cross-machine comparable key. `peers` is not redundant with the
rows: it is the only place a device with *zero* sessions can appear, which is what separates "the other
machine is up and has nothing for project X" from "the other machine has said nothing in 80 seconds".
Both arrays are always present and never null.

All the judgment lives in the pure `agent_roster`, unit-tested without a router; the handler is a thin
assembler. Two details that look arbitrary and are not: the device prefix is stripped via the row's
`origin`, never by splitting on the first `/`, because a device name may itself contain a slash; and peer
session counts filter on `!local`, because a local row carries this device's own name and `device_name`
bootstraps from the hostname, so two machines can genuinely share one.

## What review caught

Three independent reviewers converged on one defect, which is worth recording because the fix is the
whole point of the route:

**`sync_listening` was derived from config** (`sync.listen && token.is_some()`) rather than from the
running listener. It disagreed with reality three ways: an empty-string token passes `is_some()` while
startup refuses to launch on it; `sync.listen` hot-reloads while the listener is start-only; and the bind
can simply fail on a taken port. Each makes config claim a listener that is not there — and the field's
own contract is that `false` means "an empty `peers` tells you nothing". A false `true` inverts exactly
the misreading the route exists to prevent. Replaced with `sync::SyncListening`, an `AtomicBool` set at
the bind site and cleared when serving ends, so the flag records the outcome rather than re-deriving the
intent.

Also fixed: peer session counts credited local rows to a peer sharing this device's name; and a doc
comment claimed `remote_last_seen` existed to avoid `remote_snapshot`'s deep clone, which is false — the
roster's other half calls `resolved_snapshot`, which clones anyway. It exists because freshness lives on
the device, not on the session row. In a repo whose comments are the design record, a wrong *why* is a
defect.

## Accepted gap: DNS rebinding

The `Origin` guard is CSRF-only. A page whose domain is rebound to `127.0.0.1` becomes same-origin, so the
browser sends no `Origin` and the request is allowed — now a read of every project name, status and label
rather than just a forged write. Closing it means also requiring a loopback `Host`, which is **not** done:
the hook's target is overridable via `TAURI_DASHBOARD_URL`, so a host alias resolving to loopback is a
supported setup that such a check would break. Recorded in the guard's doc comment as a known, accepted
gap rather than papered over. Revisit if the roster ever carries prompt text.
