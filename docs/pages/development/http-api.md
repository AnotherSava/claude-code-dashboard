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
  "console_pids": [1234, 5678]
}
```

- `client` — identifies which adapter should handle this event. Today: `"claude"`. New clients are new server-side adapter modules; the envelope shape never grows a per-client variant.
- `event` — the agent's own event name (for Claude Code this is the `hook_event_name` field from its hook payload: `SessionStart` / `UserPromptSubmit` / `Notification` / `Stop` / `SessionEnd`).
- `payload` — opaque to the HTTP layer; forwarded verbatim to the adapter. The adapter knows what fields it cares about.
- `console_pids` — optional. Candidate pids the hook gathered — its console's process list plus its ancestor chain on Windows, the ancestor chain alone on macOS; the widget reaches the terminal through one of them to set the tab title (console attach on Windows, controlling-tty OSC write on macOS — see [Features → color terminal tabs](../features#color-terminal-tabs)). Plays no part in classification.

## Payload interpretation

The `claude` adapter parses the forwarded `payload`, maps each event to a `(status, label)` pair, and derives the row's `chat_id` from `payload.cwd`. See [Classification](classification) for the full event → status → label rules and [Features → session identity](../features#session-identity) for chat-id derivation.

## Port

The widget listens on `server_port` from `config.json` (default 9077). The Claude hook resolves its URL from `$TAURI_DASHBOARD_URL`, falling back to `http://127.0.0.1:9077`.

## Adding a new client

Writing a new adapter is a ~100 LOC pure Rust function: `src-tauri/src/adapters/<your_client>.rs` exposing `dispatch(event, payload, cfg) -> AdapterOutput`, plus a match arm in `adapters::dispatch`. See `src-tauri/src/adapters/claude.rs` for the reference implementation. No HTTP layer changes — the envelope already carries `client` as the discriminator.

## Sync API

When `sync.listen` is on (and `sync.token` set), a second listener binds **all interfaces** on `sync.listen_port` (default 9078) for dashboard-to-dashboard session sync. Every route requires `Authorization: Bearer <sync.token>`; requests without it get `401`. Implementation: `src-tauri/src/sync.rs`.

### `POST /api/sync`

A peer pushes its local sessions. The body is a full snapshot of the sender's session *metadata* (a session absent from the snapshot is removed on the receiver) plus per-session `dialog_delta` — only the dialog entries changed since that peer's last acknowledged push, since full dialogs run to hundreds of KB — plus a top-level `usage_delta` of usage-limit samples (the account-wide 5h/7d poll timeline) the receiver stores per-device and unions into its Work-intensity chart:

```json
{
  "device_name": "my-laptop",
  "listen_port": 9078,
  "delta_from": 1780789975389,
  "sessions": [
    { "session": { ...AgentSession, "dialog": [] }, "dialog_delta": [ ...DialogEntry ] }
  ],
  "usage_delta": [ ...UsageHistoryRecord ]
}
```

The `delta_from` field carries the watermark the deltas were selected against (`0` = the deltas start from the beginning of each dialog; also the default when absent). A push may carry only the oldest bounded chunk of a large backlog — the sender drains the rest in immediately following pushes, each contiguous with the last acknowledged one. Returns `204` on ingest, `400` when `device_name` is empty or equals the receiver's own. The receiver namespaces ids to `{device_name}/{id}`, stamps `origin`, and accumulates deltas — but only contiguous ones: a delta whose `delta_from` lies above everything the receiver holds for that session would leave an invisible gap below it, so it's discarded and the held dialog stays gap-free by construction (the history-window catch-up fetches the full dialog at the only moment it's read). `listen_port` plus the connection's source IP becomes the address for catch-up fetches. A device unheard from for 90 s is dropped.

### `GET /api/sync/dialog?id=<raw_id>&since=<epoch_ms>`

Catch-up: returns the *local* session's dialog entries with `timestamp > since` (the full dialog when `since` is omitted or `0`). A peer calls this when its history window opens a remote session, always for the full dialog — what it holds accumulates from push deltas (persisted per device, re-seeded after a restart) and can still have a gap *below* its newest entry, e.g. on a fresh install; the dedup merge absorbs the overlap. `404` for unknown ids.

