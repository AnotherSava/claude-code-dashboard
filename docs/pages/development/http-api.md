---
layout: default
title: HTTP API
parent: Development
nav_order: 4
---

The widget listens on `http://127.0.0.1:9077` (default) for lifecycle events from external agents. One write endpoint, one envelope shape, adapter-dispatched on the server side — plus a read-only [agent roster](#agent-roster) an agent can query to see what is running, here and on the user's other machines, and a [message relay](#cross-machine-messaging) for reaching one of those agents on another machine.

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

Two arrays, because there are two sources and they can say different amounts. `agents` is the dashboard's own tracking — a session it has classified from the hook stream, so it can say what that session is *doing*. `registry_only` is Claude Code's own list of live local sessions, read off disk on every request: proof that a session exists at a project, with no status behind it. A project present in both appears only in `agents`.

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
  ],
  "registry_only": [
    {
      "id": "printlab",
      "project": "printlab",
      "device": "air",
      "name": "printlab",
      "activity": "idle",
      "activity_age_ms": 1840200,
      "sessions": 1
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
- `status_age_ms` — time in the current status. All three arrays are always present and never `null`.

The `registry_only` entries carry a deliberately smaller vocabulary:

- `registry_only[]` — one entry per **project directory** on this machine that has at least one live interactive session and no row in `agents`; `sessions` says how many collapsed into it, so counting entries counts directories, not sessions. `null` rather than `[]` when the registry could not be read at all — see below. Always local: the registry describes this box, so there is no `local` flag (a constant carries no information) and no `last_seen_age_ms` (there is no push channel behind it). A peer's registry is not synced; its rows come from its hook stream as before.
- `id` / `project` — the same cwd derivation as an `agents` row, which is what lets a caller compare across both arrays with one pair of keys. Equal to each other here, since a local id is never namespaced.
- `name` — Claude Code's own name for the session (what the session picker shows). A different fact with a different owner than `display_name`, which is the dashboard's own rename; omitted rather than `null` when the session has none.
- `activity` — `idle` / `busy` / `unknown`, the registry's own two words plus a degrade value. **This is not a `status` and must not be read as one.** Claude Code records only idle-versus-busy, which cannot express `blocked`, `waiting` or `error` — so `busy` → `working` would not be coarse but wrong: a session parked on a question or a permission dialog has a turn in flight and is what the dashboard calls `blocked`, while `idle` covers done, errored and a settled hand-back alike. An unrecognized or missing value reads `unknown` rather than being guessed at.
- `activity_age_ms` — how long since the registry last wrote that activity. Free of skew, being this machine's own clock; omitted when the record carries no stamp.
- `sessions` — how many interactive sessions collapsed into this row. A row's identity is its directory, so two sessions in one directory (what a `--fork-session --resume` migration leaves) are one row; the freshest of them speaks for it and this number says the collapse happened.

### Judging staleness

Every time in this body is an **age**, never a timestamp: a remote row's stamps come off the sender's clock, so an absolute time would need clock agreement the two machines don't have, while every question a caller actually has is "how old is this".

A peer pushes on every state change (coalesced 300 ms) and at worst every 30 s as a heartbeat; the receiver drops a device that has been silent past a 90 s TTL, checked on that same 30 s tick — so the drop lands 90–120 s after the last push. In between, that device's rows sit in the roster with their last-pushed status **frozen**. A peer that closed its lid keeps reading `working`, and a bare `idle` is indistinguishable from a dead machine's last words.

`last_seen_age_ms` is the number to judge on. It is measured on the receiver's clock at both ends, so it carries no skew — unlike `status_age_ms`, which for a remote row is the sender's arithmetic and is only clamped at zero. A few seconds means the row was pushed on a live connection; anything past ~35 s means at least one heartbeat went missing. It is omitted for local rows, where a `0` would claim freshness on a channel that doesn't exist.

**Absence means different things on this machine and on a peer.** The dashboard learns a session exists only when that session fires a hook and restores nothing at startup, so its own tracking empties on every restart and refills on session **activity**, not on a timer. Measured across one redeploy: 1 row of 9 live sessions immediately after, 2 of 9 six minutes later, 3 of 9 later still — the idle ones had no reason to emit anything, and a session idle since before the restart stayed invisible for as long as it stayed idle. That is exactly the session a caller is most likely to be asking after.

`registry_only` closes that gap **for local sessions only**. It is read from Claude Code's own list of live sessions on every request, so it is unaffected by how long the dashboard has been up: an idle interactive session is listed the moment the dashboard starts.

**Check `registry_only` is not `null` before reading anything into a local absence.** `[]` means the list was read and this machine is running nothing; `null` means it could not be read, and the two are different answers. `null` is reached by ordinary means, not just exotic ones — no `sessions/` directory on a machine whose Claude Code predates it, an unreadable directory, or a node-based install whose records never survive the liveness check. Collapsing them would let this route assert an absence it never established, which is the one mistake a caller acting on the answer cannot recover from. When it is `[]`, a locally absent project does mean Claude Code has no live interactive session there. What a restart still costs locally is the *detail*, not the existence — a registry-only row has no status, no label and no history, because those are produced from the hook stream and nothing else.

Two things the local list still cannot see, so absence is better evidence than before but not proof: a headless session (`claude -p`) writes no registry record at all, and a session killed outright (rather than exited) can leave its record behind briefly — the dashboard checks the recorded process is still a live Claude Code process, which catches the ordinary case but not a pid recycled by another `claude`.

**For a peer's sessions the old caveat stands in full.** A peer pushes only what its own hook stream taught it — this local list is not synced — so a project missing from that device's rows may simply not have emitted anything since *that* dashboard was restarted. Its uptime is the thing to check, and it is one command run on the peer:

```bash
ps -o lstart= -p $(lsof -nP -iTCP:9077 -sTCP:LISTEN -t)
```

The route deliberately returns **facts, not a verdict**. There is no `deliverable` / `sendable` boolean: a green light computed from data that may be 90 s old would state as certain something that isn't, and the caller — which knows what it wants to do with the answer — is the one that should weigh the age. There is likewise no user-presence or idle time: a message to a peer agent starts a turn in it whether or not a human is watching that screen, so presence changes no decision.

## Cross-machine messaging

`POST /api/message` — relay one message to an agent running on **another** machine. The dashboard on this box makes an authenticated hop to the peer dashboard, which resolves the target in its own session registry and writes one frame into that session's inbox. Same loopback bind as the routes above, plus one extra gate described below.

```json
{
  "target": "chrome/transcripts",
  "text": "The token schema changed — the seq field is now required.",
  "from_agent": "tauri dashboard",
  "from_label": "Oleg's Mac — dashboard session"
}
```

- `target` — a `{device}/{project}` address, echoed from [the roster](#agent-roster). The device half is *matched* against the devices this dashboard has heard from, never split on the first `/`, because a device name may itself contain one. A bare project name is refused rather than guessed at: `project` is the cross-machine comparable key, and the same one can exist on several machines.
- `text` — the message. Capped at 64 KiB.
- `from_agent` — the caller's own chat id. A **claim**: this server is loopback and unauthenticated, so nothing about it is checked. It is carried anyway for two reasons, both below.
- `from_label` — optional free description of the sender, shown to the receiving agent alongside the claim.

### Why a local target is refused

A session on this machine gets `400` and a pointer to Claude Code's own `SendMessage`. That is not a missing feature. `SendMessage` carries a sender identity the receiving Claude Code verifies from the connecting process, and a reply address the receiver can write back to; a relayed message has neither, because the process writing the frame is a dashboard. Brokering a same-machine message would hand the caller a strictly worse version of a tool it already has.

### The sender's identity is presented as unverified

The receiving Claude Code stamps the writing process, which is the peer dashboard — not the agent that composed the message. There is no way around that short of one machine reading another's credentials, which this design exists to avoid. So the originating agent's name travels **inside the message body**, above the text, labelled `UNVERIFIED` and telling the receiving model not to treat it as authorization. The claim also becomes the frame's sender address, which is what gives each originating agent its own rate-limit and repeat-detection slot on the receiver instead of every relayed message sharing one.

### The receipt says what was observed, not what was achieved

The reply is a receipt. Its `outcome` is one of five words, and none of them is "delivered":

| `outcome` | what it promises |
|---|---|
| `written` | the peer connected to the target session's inbox and wrote the whole frame — a live listener owned by that session accepted the bytes |
| `duplicate` | this `message_id` was already written by the peer inside the dedupe window, so nothing was written now |
| `refused` | we declined before anything reached a socket; `reason` says which rule |
| `unreachable` | nothing was written — the hop could not connect, or the peer found nothing listening at the inbox |
| `unknown` | the hop's request went out and its answer was lost; the frame **may or may not** have been written |

`written` is deliberately not "delivered". A raw writer to a Claude Code inbox can distinguish exactly two states — nothing is listening, and a listener accepted our bytes. Whether the frame parsed, survived the receiver's rate limiting, or was ever shown to the model is invisible: the receiver reports those outcomes on a *separate connection back to the sender's own inbox*, which a relay that binds none never receives. Every receipt carries an `observed` sentence saying this in plain words, so a caller that only prints the receipt cannot accidentally claim delivery.

`unknown` answers `200`, not `5xx`, on purpose: a `5xx` reads as "it failed, retry", and retrying a message that may already have been written is how one message becomes two.

`reason` values: `local_target`, `not_an_address`, `unknown_device`, `device_unheard`, `no_device_name`, `empty_text`, `too_large`, `csrf`, `no_such_session`, `ambiguous_target`, `registry_unreadable`, `no_inbox`, `inbox_dead`, `peer_unreachable`, `response_lost`. A refusal the *peer* made keeps its own status across the relay — `no_such_session` stays a `404`, `ambiguous_target` a `409` — so a caller is not told its request was malformed when the problem was on the other machine.

Two refusals are the sender's own and are made with certainty: a local target, and a device it holds no address for (the refusal lists the devices it does know, and whether it is listening for peers at all). Everything else — above all *does that project exist over there* — is the receiving dashboard's answer, relayed verbatim, because this side's roster is at best one push cycle old.

### The extra gate on this route

The [`Origin` check](#endpoint) shared by the hook routes is CSRF only; a page whose domain is rebound to `127.0.0.1` becomes same-origin and sends no `Origin` at all. That gap is accepted for a status write and a roster read. It is not acceptable for a route that starts a turn inside a live agent on another machine, so this one **also requires a loopback `Host`** — `127.0.0.1`, `::1` or `localhost`. A rebound page carries the attacker's own hostname there. The hook routes keep the accepted gap, because the `TAURI_DASHBOARD_URL` host alias they support is a real setup and their stake is unchanged.

### Idempotency

Each send is stamped with a `message_id` minted here, and the *receiving* dashboard — the only party that knows whether bytes reached a socket — remembers the ids it has written for 10 minutes. A hop that times out after the peer already wrote the frame can therefore be retried without delivering twice. The entry expires, so a deliberate resend of the same words an hour later still gets through.

Claude Code's own identical-repeat drop is not relied on: it compares only against the immediately previous message from one sender, so an interleaved message defeats it; its window is tunable from the server side; it drops a legitimate resend as readily as a retry; and it is invisible to a relay.

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


### `POST /api/sync/message`

The far half of [cross-machine messaging](#cross-machine-messaging), and the only sync route that writes outside the remote array. A peer relays one message; this dashboard resolves the target in **its own** session registry, reads that session's messaging key as the user who owns it, and writes one frame into its inbox.

```json
{
  "origin_device": "air",
  "message_id": "air-1756500000000-7",
  "target_project": "transcripts",
  "from_agent": "tauri dashboard",
  "from_label": "Oleg's Mac — dashboard session",
  "text": "The token schema changed — the seq field is now required."
}
```

Every field is optional on the wire (`serde(default)`) so a peer on an older build parses a newer envelope rather than failing the whole request; the handler validates the three it cannot work without and answers `400`.

Returns `200` and a receipt for anything it observed about the socket (`written`, `duplicate`, `unreachable`), and a `4xx` with a receipt for an envelope or target problem: `404` when no live interactive session on this machine derives that project id, `409` when two do. Two sessions in one directory are two inboxes and there is no way to choose between them, so it refuses rather than picking — the same rule terminal titles already follow, where the stake was only which tab got a title.

**No credential crosses the wire.** The alternative shape — reaching into the other machine to read a session's key file and injecting it into that machine's IPC channel — is indistinguishable from exfiltration whatever the intent, and was refused when tried. This design exists so the only process that ever reads a messaging key is the dashboard already running as that user on that machine.

**Windows is unverified.** Three things are open and are not assumed: that the Windows build writes the session registry at all, that it publishes `messagingSocketPath`, and the exact framing its named pipe requires. What is known is that the auth line is *required* there and that a connection whose first line is not valid auth is closed without a word — indistinguishable from a dead session. So on Windows even a `written` outcome is weaker than on macOS: the bytes left us, and nothing more. The socket path is always taken verbatim from the session's own record rather than composed from a pattern, which is what lets the Windows leg follow a path Claude Code chose for itself.
