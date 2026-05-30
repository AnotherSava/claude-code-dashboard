---
layout: default
title: HTTP API
parent: Development
nav_order: 4
---

The widget listens on `http://127.0.0.1:9077` (default) for lifecycle events from external agents. One endpoint, one envelope shape, adapter-dispatched on the server side.

### Endpoint

`POST /api/event` with `Content-Type: application/json`. Returns `204 No Content` on success, `403` if the `Origin` header is a real web origin (blocks browser XHR), `400` on malformed JSON.

### Envelope

```json
{
  "client": "claude",
  "event": "UserPromptSubmit",
  "payload": { ... raw agent payload ... }
}
```

- `client` — identifies which adapter should handle this event. Today: `"claude"`. New clients are new server-side adapter modules; the envelope shape never grows a per-client variant.
- `event` — the agent's own event name (for Claude Code this is the `hook_event_name` field from its hook payload: `SessionStart` / `UserPromptSubmit` / `Notification` / `Stop` / `SessionEnd`).
- `payload` — opaque to the HTTP layer; forwarded verbatim to the adapter. The adapter knows what fields it cares about.

### Claude Code events

The `claude` adapter recognizes five events. Other event names are silently ignored.

| `event`             | Derived status                                                                                                                | Label source                                            |
|---                  |---                                                                                                                            |---                                                      |
| `SessionStart`      | `idle`                                                                                                                        | —                                                       |
| `UserPromptSubmit`  | `working`                                                                                                                     | `payload.prompt` (whitespace-collapsed, chrome-stripped)|
| `Notification`      | `awaiting` (usually); `done` if `notification_type == "idle_prompt"` and the last assistant turn doesn't end with `?`         | `"needs approval: <tool>"` / `"plan approval"` / the raw `message` (truncated to 60 chars) |
| `Stop`              | `done`; flips to `awaiting` if the last assistant turn ends with `?` (minus configured benign closers)                        | `"has a question"` when flipped                         |
| `SessionEnd`        | — (emits a `clear`, removing the row)                                                                                         | —                                                       |

The adapter derives a friendly `chat_id` from `payload.cwd` and the `projects_root` config setting; see [Features → session identity](../features#session-identity) for chat-id rules.

### Sticky label state machine

A session's *display* label is not always the latest `label` produced by the adapter — an approval cycle keeps the original task visible, while a new task captures a fresh prompt. See [Sticky labels](sticky-labels) for the full state machine and display rules.

### Port

The widget listens on `server_port` from `config.json` (default 9077). The Claude hook resolves its URL from `$TAURI_DASHBOARD_URL`, falling back to `http://127.0.0.1:9077`.

### Adding a new client

Writing a new adapter is a ~100 LOC pure Rust function: `src-tauri/src/adapters/<your_client>.rs` exposing `dispatch(event, payload, cfg) -> AdapterOutput`, plus a match arm in `adapters::dispatch`. See `src-tauri/src/adapters/claude.rs` for the reference implementation. No HTTP layer changes — the envelope already carries `client` as the discriminator.

