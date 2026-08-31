---
layout: default
title: HTTP API
parent: Development
nav_order: 4
---

The widget listens on `http://127.0.0.1:9077` (default) for lifecycle events from external agents. One write endpoint, one envelope shape, adapter-dispatched on the server side — plus a read-only [agent roster](#agent-roster) an agent can query to see what is running, here and on the user's other machines.

A second, separate listener serves the [multi-device sync](#sync-api) API when enabled — the hook API below stays loopback-only and unauthenticated regardless.

## Endpoint

`POST /api/event` with `Content-Type: application/json`. Returns `204 No Content` on success, `403` if the `Origin` header is a real web origin (blocks browser XHR), `400` on malformed JSON.

## Envelope

```json
{
  "client": "claude",
  "event": "UserPromptSubmit",
  "payload": { ... raw agent payload ... },
  "console_pids": [1234, 5678],
  "agent_pid": 4321
}
```

- `client` — identifies which adapter should handle this event. Today: `"claude"`. New clients are new server-side adapter modules; the envelope shape never grows a per-client variant.
- `event` — the agent's own event name (for Claude Code this is the `hook_event_name` field from its hook payload: `SessionStart` / `UserPromptSubmit` / `Notification` / `Stop` / `SessionEnd`).
- `payload` — forwarded verbatim to the adapter, which knows what fields it cares about. The HTTP layer reads exactly one field out of it directly: `session_id`, which locks a row to a session across a mid-conversation `cd` (`ChatIdRegistry::resolve`) and decides who may end that row (`ChatIdRegistry::claim` + `http_server::clear_permitted`).
- `console_pids` — optional. Candidate pids the hook gathered — its console's process list plus its ancestor chain on Windows, the ancestor chain alone on macOS, stopping at the owning Claude Code process. These are now a **fallback**: the widget prefers to find the row's terminal in Claude Code's own list of live sessions, and only falls back to these pids when that list can't place the row (see [Features → color terminal tabs](../features#color-terminal-tabs)). Empty is a valid answer; the widget then writes no title. Plays no part in classification.
- `agent_pid` — optional. The pid of the owning Claude Code process — the image (`claude` / `claude.exe`) the `console_pids` walk stopped on, so both fields come off one pass over the process table. The widget tracks it so it can remove a row whose session exited without a `SessionEnd` (see [Features → live status](../features#live-status)). `null` when the hook can't identify it (e.g. a node-based install, whose chain is then unbounded). Plays no part in classification.

## Payload interpretation

The `claude` adapter parses the forwarded `payload`, maps each event to a `(status, label)` pair, and derives the row's `chat_id` from `payload.cwd`. See [Classification](classification) for the full event → status → label rules and [Features → session identity](../features#session-identity) for chat-id derivation.

## Port

The widget listens on `server_port` from `config.json` (default 9077). The Claude hook resolves its URL from `$TAURI_DASHBOARD_URL`, falling back to `http://127.0.0.1:9077`.

## Adding a new client

Writing a new adapter is a ~100 LOC pure Rust function: `src-tauri/src/adapters/<your_client>.rs` exposing `dispatch(event, payload, cfg) -> AdapterOutput`, plus a match arm in `adapters::dispatch`. See `src-tauri/src/adapters/claude.rs` for the reference implementation. No HTTP layer changes — the envelope already carries `client` as the discriminator.

## Agent roster

`GET /api/agents` — the sessions this dashboard tracks, on this machine *and* synced from peers, as one merged list. It exists because Claude Code's own session listing sees only the local machine; the dashboard already merges both worlds, so it can answer "does a session for project X exist, on which machine, in what state, and how fresh is that answer?" without reaching the other box.

Read-only: it mutates nothing, emits no event, writes no `decision` line, and takes no query parameters. Same loopback bind and same `Origin` guard as `POST /api/event` — `403` for a real browser origin, `500` if the app's state isn't up yet. Like `server_port` itself, the route is wired once at startup, so a new build needs an app restart before it answers.

```json
{
  "device": "air",
  "sync_listening": true,
  "peers": [ { "device": "chrome", "last_seen_age_ms": 4120, "sessions": 2 } ],
  "agents": [
    {
      "id": "tauri dashboard",
      "project": "tauri dashboard",
      "device": "air",
      "local": true,
      "status": "working",
      "label": "add the /api/agents route",
      "status_age_ms": 18400
    },
    {
      "id": "chrome/transcripts",
      "project": "transcripts",
      "device": "chrome",
      "local": false,
      "display_name": "transcripts (win)",
      "status": "blocked",
      "label": "Should I drop the legacy column?",
      "status_age_ms": 236500,
      "last_seen_age_ms": 4120
    }
  ]
}
```

- `device` — the machine being queried, so one merged roster is self-describing. `null`, never a stand-in name, when `sync.device_name` was never bootstrapped.
- `sync_listening` — whether a sync listener is actually bound and serving right now. Deliberately the *running* state rather than the config that was meant to produce it: an empty-string token, a `sync.listen` toggled after startup, and a failed bind each leave the config saying yes while nothing listens. When `false`, an empty `peers` says nothing about the other machine; don't read it as "no sessions there".
- `peers[]` — one entry per synced device, whether or not it currently has rows. This is the only place a live-but-idle machine can appear, which is what separates "the peer is up and has nothing for project X" from "the peer has said nothing in 80 s".
- `id` — the dashboard-canonical id, namespaced exactly as everything else here addresses a row (`chrome/transcripts`).
- `project` — the same id with the device prefix stripped (identical to `id` for a local row). Chat ids are derived from the cwd with backslashes normalized and the projects root removed, so one project yields the same string on macOS and on Windows — this is the field to compare across machines.
- `device` / `local` — which machine the session runs on, and the authoritative local test. Kept separate because `device` can be `null` for an unnamed local box.
- `display_name` — omitted rather than `null` when unset, so a caller falls back to `id` exactly as the app does.
- `status` — `idle` / `working` / `waiting` / `blocked` / `error` / `done`, the same values [Classification](classification) assigns.
- `label` — the "what is it doing" line the dashboard row itself shows.
- `status_age_ms` — time in the current status. Both arrays are always present and never `null`.

### Judging staleness

Every time in this body is an **age**, never a timestamp: a remote row's stamps come off the sender's clock, so an absolute time would need clock agreement the two machines don't have, while every question a caller actually has is "how old is this".

A peer pushes on every state change (coalesced 300 ms) and at worst every 30 s as a heartbeat; the receiver drops a device that has been silent past a 90 s TTL, checked on that same 30 s tick — so the drop lands 90–120 s after the last push. In between, that device's rows sit in the roster with their last-pushed status **frozen**. A peer that closed its lid keeps reading `working`, and a bare `idle` is indistinguishable from a dead machine's last words.

`last_seen_age_ms` is the number to judge on. It is measured on the receiver's clock at both ends, so it carries no skew — unlike `status_age_ms`, which for a remote row is the sender's arithmetic and is only clamped at zero. A few seconds means the row was pushed on a live connection; anything past ~35 s means at least one heartbeat went missing. It is omitted for local rows, where a `0` would claim freshness on a channel that doesn't exist.

**A present row is strong evidence; what an absent row is worth depends on how long the dashboard has been up.** The dashboard learns a session exists only when that session fires a hook, and it restores nothing at startup — so a restart empties the roster and it refills as sessions emit events. On a dashboard up for days, which is the normal case since it is restarted only to deploy it, nearly everything live has emitted something and an absent row genuinely suggests no such session or that device's dashboard being down. Fresh from a restart it means nothing.

The uptime is one command, so check it rather than assuming either way:

```bash
ps -o lstart= -p $(lsof -nP -iTCP:9077 -sTCP:LISTEN -t)
```

What makes the post-restart window worse than "wait a minute" is that the gap closes on session **activity**, not on a timer. Measured across one redeploy: 1 row of 9 live sessions immediately after, 2 of 9 six minutes later, 3 of 9 later still — the idle ones had no reason to emit anything. A session idle since before the dashboard started stays invisible for as long as it stays idle, however long the dashboard has been up, and that is exactly the kind of session a caller is most likely to be asking after. Fall back to another check when uptime is short or the target has been sitting idle.

The route deliberately returns **facts, not a verdict**. There is no `deliverable` / `sendable` boolean: a green light computed from data that may be 90 s old would state as certain something that isn't, and the caller — which knows what it wants to do with the answer — is the one that should weigh the age. There is likewise no user-presence or idle time: a message to a peer agent starts a turn in it whether or not a human is watching that screen, so presence changes no decision.

## Sync API

When `sync.listen` is on (and `sync.token` set), a second listener serves `sync.listen_port` (default 9078) for dashboard-to-dashboard session sync. Two gates sit in front of every route, in this order: the connection's source address must be inside `sync.bind_scope` — by default this device's tailnet (100.64.0.0/10, fd7a:115c:a1e0::/48) plus loopback, anything else gets `403` — and the request must carry `Authorization: Bearer <sync.token>`, or it gets `401`. Under the default scope the listener binds only this device's Tailscale addresses and loopback; if no Tailscale address is found at startup it binds all interfaces instead, logs that at `warn`, and keeps refusing non-tailnet sources. Implementation: `src-tauri/src/sync.rs`.

### `POST /api/sync`

A peer pushes its local sessions. The body is a full snapshot of the sender's session *metadata* (a session absent from the snapshot is removed on the receiver), with each session's dialog reduced to a `dialog_tip` and the account-wide 5h/7d usage timeline to a `usage_tip`. Content is fetched by the receiver afterwards, so a push stays small no matter how far behind a peer is:

```json
{
  "device_name": "my-laptop",
  "listen_port": 9078,
  "sessions": [
    { "session": { ...AgentSession, "dialog": [] }, "dialog_tip": 1780789975389 }
  ],
  "usage_tip": 1780789975389
}
```

The push carries **no dialog or usage content** — only a full metadata snapshot plus, per session, the `dialog_tip` (the sender's newest dialog timestamp) and a device-wide `usage_tip`. The receiver compares each tip against what it already holds and fetches the difference itself from the `GET` endpoints below. This keeps the sender stateless: the same body goes to every peer on every cycle, so nothing has to be remembered about a peer's progress and a failed push costs only a retry.

Returns `204` on ingest, `400` when `device_name` is empty or equals the receiver's own, `401` without a valid bearer token, `403` from a source outside `sync.bind_scope`. The receiver namespaces ids to `{device_name}/{id}`, stamps `origin`, and carries over the dialog it has already accumulated (persisted per device, re-seeded after a restart). `listen_port` plus the connection's source IP becomes the address it pulls from. A device unheard from for 90 s is dropped.

### `GET /api/sync/dialog?id=<raw_id>&since=<epoch_ms>`

Returns the *local* session's dialog entries with `timestamp > since` (the full dialog when `since` is omitted or `0`). This is the routine content path: on every push a peer asks for the range above its own newest held entry. The history window additionally asks for the *full* dialog when it opens a remote session — the dedup merge absorbs the overlap, and it covers the one case a `since` cannot express (an entry the merge dropped as an apparent re-read sits below the newest held timestamp). `404` for unknown ids.

### `GET /api/sync/usage?since=<epoch_ms>`

Returns the *local* usage-limit samples with `ts > since` (all of them when `since` is omitted or `0`). The usage counterpart of the dialog pull, requested when a push advertises a `usage_tip` newer than what the receiver holds for that device. Gives `remote_usage/` a repair path of its own: a peer that lost its copy asks again, rather than waiting for the origin to restart.

