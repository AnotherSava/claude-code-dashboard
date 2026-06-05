---
layout: default
title: HTTP API
parent: Development
nav_order: 4
---

The widget listens on `http://127.0.0.1:9077` (default) for lifecycle events from external agents. One endpoint, one envelope shape, adapter-dispatched on the server side.

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
- `console_pids` — optional. Candidate pids the hook gathered — its console's process list plus its ancestor chain on Windows, the ancestor chain alone on macOS; the widget reaches the terminal through one of them to set the tab title (console attach on Windows, controlling-tty OSC write on macOS — see [Features → terminal tab titles](../features#terminal-tab-titles)). Plays no part in classification.

## Payload interpretation

The `claude` adapter parses the forwarded `payload`, maps each event to a `(status, label)` pair, and derives the row's `chat_id` from `payload.cwd`. See [Classification](classification) for the full event → status → label rules and [Features → session identity](../features#session-identity) for chat-id derivation.

## Port

The widget listens on `server_port` from `config.json` (default 9077). The Claude hook resolves its URL from `$TAURI_DASHBOARD_URL`, falling back to `http://127.0.0.1:9077`.

## Adding a new client

Writing a new adapter is a ~100 LOC pure Rust function: `src-tauri/src/adapters/<your_client>.rs` exposing `dispatch(event, payload, cfg) -> AdapterOutput`, plus a match arm in `adapters::dispatch`. See `src-tauri/src/adapters/claude.rs` for the reference implementation. No HTTP layer changes — the envelope already carries `client` as the discriminator.

