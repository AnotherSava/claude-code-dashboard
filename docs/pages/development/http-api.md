---
layout: default
title: HTTP API
parent: Development
nav_order: 4
---

The widget listens on `http://127.0.0.1:9077` (default) for lifecycle events from external agents. One endpoint, one envelope shape, adapter-dispatched on the server side.

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
- `console_pids` — optional. Candidate pids the hook gathered — its console's process list plus its ancestor chain on Windows, the ancestor chain alone on macOS; the widget reaches the terminal through one of them to set the tab title (console attach on Windows, controlling-tty OSC write on macOS — see [Features → color terminal tabs](../features#color-terminal-tabs)). The chain stops at the owning Claude Code process, so a session that owns no terminal of its own — a Claude Code desktop-app session, whose ancestors are whatever terminal launched it — reports only unreachable pids instead of titling an unrelated session's tab. Empty is a valid answer; the widget then writes no title. Plays no part in classification.
- `agent_pid` — optional. The pid of the owning Claude Code process — the image (`claude` / `claude.exe`) the `console_pids` walk stopped on, so both fields come off one pass over the process table. The widget tracks it so it can remove a row whose session exited without a `SessionEnd` (see [Features → live status](../features#live-status)). `null` when the hook can't identify it (e.g. a node-based install, whose chain is then unbounded). Plays no part in classification.

## Payload interpretation

The `claude` adapter parses the forwarded `payload`, maps each event to a `(status, label)` pair, and derives the row's `chat_id` from `payload.cwd`. See [Classification](classification) for the full event → status → label rules and [Features → session identity](../features#session-identity) for chat-id derivation.

## Port

The widget listens on `server_port` from `config.json` (default 9077). The Claude hook resolves its URL from `$TAURI_DASHBOARD_URL`, falling back to `http://127.0.0.1:9077`.

## Adding a new client

Writing a new adapter is a ~100 LOC pure Rust function: `src-tauri/src/adapters/<your_client>.rs` exposing `dispatch(event, payload, cfg) -> AdapterOutput`, plus a match arm in `adapters::dispatch`. See `src-tauri/src/adapters/claude.rs` for the reference implementation. No HTTP layer changes — the envelope already carries `client` as the discriminator.

## Sync API

When `sync.listen` is on (and `sync.token` set), a second listener binds **all interfaces** on `sync.listen_port` (default 9078) for dashboard-to-dashboard session sync. Every route requires `Authorization: Bearer <sync.token>`; requests without it get `401`. Implementation: `src-tauri/src/sync.rs`.

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

Returns `204` on ingest, `400` when `device_name` is empty or equals the receiver's own, `401` without a valid bearer token. The receiver namespaces ids to `{device_name}/{id}`, stamps `origin`, and carries over the dialog it has already accumulated (persisted per device, re-seeded after a restart). `listen_port` plus the connection's source IP becomes the address it pulls from. A device unheard from for 90 s is dropped.

### `GET /api/sync/dialog?id=<raw_id>&since=<epoch_ms>`

Returns the *local* session's dialog entries with `timestamp > since` (the full dialog when `since` is omitted or `0`). This is the routine content path: on every push a peer asks for the range above its own newest held entry. The history window additionally asks for the *full* dialog when it opens a remote session — the dedup merge absorbs the overlap, and it covers the one case a `since` cannot express (an entry the merge dropped as an apparent re-read sits below the newest held timestamp). `404` for unknown ids.

### `GET /api/sync/usage?since=<epoch_ms>`

Returns the *local* usage-limit samples with `ts > since` (all of them when `since` is omitted or `0`). The usage counterpart of the dialog pull, requested when a push advertises a `usage_tip` newer than what the receiver holds for that device. Gives `remote_usage/` a repair path of its own: a peer that lost its copy asks again, rather than waiting for the origin to restart.

