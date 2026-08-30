# Peer messaging: the native channel, and making the dashboard see it

Rewritten 2026-08-30. The original brief (2026-08-29) designed a broker that would deliver messages by
typing into agterm panes. That premise is gone: **Claude Code ships cross-session messaging as a
documented feature**, it is on across all eight sessions, and it already does the delivery, framing,
serialization and loop-breaking the broker existed to provide.

What remains is not delivery. It is that this dashboard cannot see peer traffic — and currently
**mis-records it as the user's own work**.

## What is true now

Cross-session messaging is documented at `code.claude.com/docs/en/cross-session-messaging.md`. The
mechanics, verified on this machine:

- Each session binds an inbox socket and publishes `messagingSocketPath` in its
  `<claude-config>/sessions/<pid>.json` record. All eight local sessions have one.
- `ListAgents` lists reachable peers by name; `SendMessage` delivers plain text to one. `ListAgents` is
  a normal loaded tool; `SendMessage` is deferred, so the model calls `ToolSearch` before its first send.
- Delivery into an idle session **starts a new turn**; mid-turn it arrives between tool calls, so a
  running tool is never interrupted.
- Claude Code prepends its own framing: the receiver is told the text came from another session and not
  from the user, that it cannot approve anything or change configuration, and a slash command inside the
  text arrives inert.
- Anti-chatter is built in: per-sender rate limiting, identical-repeat dropping in a short window, a
  50-message queue cap, and burst refusal *at the sender*. Per the docs, a message loop between two
  sessions "stops on its own".
- Availability needed 2.1.248+ for same-machine delivery with feature-flag fetching off, which is this
  machine's configuration. The sessions were on 2.1.238; restarting them onto 2.1.251 turned it on with
  no settings change.

Two things outside this repo were done alongside this rewrite, and the plan assumes them:

- The `claude` shell function passes `--name "${PWD##*/}"`, so a session is addressable as `scheduler`
  rather than a derived `scheduler-30` that changes on every restart. It is skipped when the argv
  already names one, because agterm's restore types the captured argv back and would otherwise stack a
  `--name` per restart.
- A global memory entry plus its index line carry *when* to send — the judgement Claude Code cannot
  supply. Its built-in limits stop a runaway loop, not eight agents being sociable.

## The defect: a peer message is recorded as the user's task

This is the reason the plan still exists, and it is worth stating precisely because it is invisible
until you look for it.

`UserPromptSubmit` fires for a peer-delivered message exactly as it does for a typed prompt. The full
payload, captured from a throwaway session that received one:

```
session_id, transcript_path, cwd, prompt_id, permission_mode,
hook_event_name, prompt, session_title
```

There is **no peer marker of any kind**, and `prompt` carries the peer's raw text without Claude Code's
framing wrapper. So `adapters/claude.rs` takes it as a user prompt: `is_system_injected` only filters a
`<task-notification>` prefix, so the text passes through, becomes a `DialogRole::User` dialog entry, and
sets the row's `label` and `original_prompt`. A peer's message therefore **replaces the user's task
label**, and the history window attributes the peer's words to the user.

The transcript tells the truth. The same message's transcript entry carries:

```
isMeta: true
origin: { kind: "peer", from: "<sender name>" }
promptId: <the same id the hook reported as prompt_id>
```

So the signal exists, on the path `log_watcher` already tails, and it correlates to the hook event by a
shared id rather than by a heuristic. `log_watcher` does not read `isMeta` or `origin` today.

## What to build

Three pieces, smallest first. Only the first is a correctness fix; the rest is observability.

### 1. Stop attributing peer messages to the user

`log_watcher` reads `isMeta` + `origin.kind == "peer"` + `promptId` on user entries. On a peer entry it
tells `state` to undo the task boundary the hook created for that `promptId`: restore the prior `label`
and `original_prompt`, and keep `task_started_at`. Log it under a new `decision = "peer_message"` with
the sender in `reason`, so `/investigate` can answer "why did this row's task change?".

This is a correction after the fact, which this project normally resists. It is the right shape anyway,
because the alternative — deferring every row's task boundary until the transcript confirms it — would
slow the normal path to fix the rare one. If a future Claude Code adds an origin field to the
`UserPromptSubmit` payload, adopt it and delete the correction outright, per the project's
latest-version-only convention.

### 2. Render a peer message as a peer message

Add a `DialogRole::Peer` variant carrying the sender name, rather than a boolean beside `User` — the
sender is information the role should hold, and a flag whose only job is to say what another field means
is the shape this project avoids. The history window renders it distinctly; the row preview attributes
it. `prompt_history.json` gains the variant, so old entries stay `User` and need no migration.

Decide at this point whether a peer message should suppress the Telegram ping that a normal prompt-to-
completion cycle produces. A peer exchange the user never asked for probably should not notify them at
2am, but a `Blocked` outcome from one probably should.

### 3. See outbound sends

Nothing currently records that *this* session sent anything. `PreToolUse` is matcher-gated to
`AskUserQuestion|ExitPlanMode`; adding `SendMessage` to that matcher gives the outbound half for the cost
of one matcher entry, with the recipient and text in the payload. Store it as a `DialogRole::Peer` entry
marked outbound.

With both halves recorded, the dashboard becomes the one place the whole conversation between agents is
visible — which is the thing the original broker was really for, minus the delivery machinery.

## Cross-machine (Mac ↔ Windows): deferred, and the two routes

Claude Code supports messaging a session on another machine natively, but the message travels **through
Anthropic's servers**, arriving over that machine's Remote Control connection, and needs a claude.ai
sign-in as the active authentication on both ends. This machine sets
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, which stands in the way; whether Remote Control works with
it set is untested. `isolatePeerMachines: true` exists to require explicit approval before any message
leaves the machine.

The alternative keeps traffic on the tailnet: relay through this app's existing sync transport, which
already carries session state between the two devices with a bearer token, peer discovery, a 90s
liveness TTL and disk persistence. The far side's last hop would be a write into Claude Code's own inbox
socket — Windows uses a named pipe and **requires** an auth line as the connection's first frame, read
from the session's exported `CLAUDE_CODE_MESSAGING_TOKEN`.

Neither is worth starting until the local half has run long enough to show whether cross-machine
messaging is a real need. Two things would have to be settled first: the sync listener currently binds
`0.0.0.0` with a single shared token compared non-constant-time, which is acceptable for a read-only
surface and not for one that can start a turn inside a live agent; and nothing has confirmed that the
Windows build writes the session registry or binds a pipe at all.

## Superseded — what the original brief got wrong, and why

Kept deliberately, so the reasoning is not rediscovered later.

- **"Delivery is not reliable — this is the crux."** It was the crux of typing into a terminal. Claude
  Code delivers over a socket and reports held / delivered / refused back to the sender. The whole
  section, the newline stripping, the separate `\r`, the pane read-back and the "typed but unconfirmed"
  state are gone.
- **agterm mechanics for delivery.** Not needed. agterm keeps its read commands for diagnosis.
- **The agterm-session ↔ `chat_id` join.** Unnecessary. A session is addressed by a name from
  `ListAgents`; `session_registry` already maps cwd → pid for the local side.
- **Decision 2, notification-not-payload.** Reversed. Send the body: Claude Code's own framing already
  prevents the payload being read as a user instruction, and does it better than a fixed notification
  line would.
- **The anti-chatter policy section.** Built into Claude Code, except the judgement of when to send at
  all, which now lives in global memory rather than in this app.
- **"Serializes per target."** Claude Code's prompt queue does it.
- **A `peer` CLI and a `peer-chat` skill.** Both obsolete: the tools are native, and `ListAgents` proves
  itself in one call.
- **Open questions 1, 2 and 4** (identity join, which state is deliverable, where a notification goes if
  a session never goes idle) are all answered by the native channel. Questions 3 and 5 — restart
  survival and whether the message log is a first-class UI surface — survive as part of the
  observability work above.

The constraints about not re-adding agterm's status hooks and not editing the vendored agterm skill
still stand, and are unaffected by any of this.
