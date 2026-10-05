---
layout: default
title: Sticky labels
parent: Development
nav_order: 2
---

How the widget keeps a meaningful caption next to a session row across the whole approval-cycle dance — and what determines whether you see the *task* you asked for or the *thing happening right now*.

## Two fields, two questions

Each session row carries two pieces of human-readable text on `AgentSession` (`src-tauri/src/state.rs`):

| Field             | Question it answers                              | Lifetime                                                  |
|---                |---                                               |---                                                        |
| `original_prompt` | "What is this session working on?"               | Captured at task start, sticky until the next task starts |
| `label`           | "What's the latest thing that just happened?"    | Overwritten by every event                                |

`label` is the **transient now**: it jitters with whatever the agent or user just did. `original_prompt` is the **persistent task identity**: pinned to what the user asked for at the top of a task, surviving every approval round-trip until a new task explicitly begins.

Both fields exist because Claude Code emits a flurry of events during a single user task — the user's prompt, every permission ask, every "y", every internal Stop with a clarifying question — and each one carries its own snippet of text. Showing only the most recent text would mean staring at `"y"` or `"needs approval: Bash"` instead of remembering what you actually asked for; showing only the original prompt would hide the in-the-moment information you need (like the question being asked or an error message).

## How `label` is set

`label` is set by the per-client adapter on every event. The Claude adapter's full list of `(event, status, label)` mappings — including how box-drawing chrome gets stripped from messages and how the 60-character truncation works — lives in [Classification](classification#event--status).

One detail relevant to the state machine: when the adapter emits `None` for the label, the state layer keeps the **prior** `label` value rather than blanking it (`src-tauri/src/label_policy.rs`). This is how `Stop` and `Notification` events that carry no label of their own (just a status change) leave the previous text in place.

## How `original_prompt` is set

The state layer (`src-tauri/src/state.rs::apply_set` via `src-tauri/src/label_policy.rs::select`) decides what happens to `original_prompt` on every `set` event. The decision depends on whether the session already exists, the prior status, and the new status:

| Existing session? | Prior status                      | New status | Action on `original_prompt`                                                  |
|---                |---                                |---         |---                                                                           |
| no                | —                                 | `working`  | set to incoming `label`                                                      |
| no                | —                                 | anything   | leave `None`                                                                 |
| yes               | `done` / `idle` / `working` / `waiting` | `working`  | **re-capture** to incoming `label` (a new task is starting); also reset `working_accumulated_ms = 0` and `state_entered_at = now` — *unless the incoming `label` is a [continuation prompt](#continuation-prompts), in which case the boundary is suppressed and the row is treated as if it were an approval cycle* |
| yes               | `blocked`                        | `working`  | leave pinned (approval cycle: agent asked, user answered)                    |
| yes               | any other                         | any        | leave pinned                                                                 |

The third row is the **task boundary**: a transition into `working` from any status *except* `blocked` counts as a new task, unless the prompt names no task: a continuation prompt, a prompt Claude Code submitted on its own account, or one of the messages from other agents described after these cases.

- `done` / `idle` / `waiting` → `working` is the natural case: the last turn ended, the row is clean after a `/clear` or a fresh start, or background work was still finishing — and the user is starting something new either way.
- `working` → `working` covers the **cancellation case**: the user hit `Esc` mid-task and submitted a fresh prompt before the agent could emit a `Stop`. Without this rule the row would still display the cancelled prompt, which is misleading.
- `blocked` → `working` is **never** a boundary, whatever the prompt. It's the canonical approval cycle (agent asks → user answers → agent resumes), so typing `y` doesn't clobber the original prompt.

If the new event has `label: None` on a task boundary, the prior `original_prompt` survives unchanged.

Two kinds of prompt Claude Code submits on its own account are no task from anyone: a subagent handing its report back (`<agent-message from="…">` with `[Subagent hand-back]` on the next line) and a cross-session notice (`[Cross-session idle notice]`, `[Cross-session delivery notice]`). The adapter gives them no label (`peer_message::parse_harness_prompt`, logged in the `classify` line's `reason`), and `apply_set` does not count them as a boundary. The turn still shows `working` and the history still records the prompt, but the row keeps its task, label and working timer, and the entry is not marked as a task start.

A message from another agent that arrives while the row's turn is still `working` or `waiting` is no boundary either: it is a reply to, or a follow-up on, the exchange already under way, so the person's task stays on the row. A relayed reply to a message this row's agent sent is no boundary on a `done` or `idle` row either, which is where it usually lands, since nothing polls for it and the asking turn has ended by then; the relay header's "This is a reply to your message" line is what marks it. Any other agent message arriving on a `done` or `idle` row starts a task like any prompt — a reply through Claude Code's own `SendMessage` included, because its envelope carries no reply mark.

### Restore from disk

When a session is re-created from `prompt_history.json` — after an app restart, or when `/clear` ends and immediately re-starts the session under the same cwd-derived id — `apply_set` seeds `original_prompt` from the persisted value, so an in-flight task survives a restart. The exception: if the restored dialog ends in a **separator** (the boundary marker `/clear` and `/compact` leave behind), the conversation has been cleared, so the row starts clean — `original_prompt` and `task_started_at` are dropped while the dialog history is kept for the History window. A `working` prompt arriving on the same event still takes precedence and starts a real task.

### Cancelled turns revert to the prior status

A turn cancelled with Esc fires no lifecycle hook. The transcript watcher (the `[Request interrupted by user]` marker) calls `state::revert_cancelled_turn`, which settles the row back to `AgentSession::status_before_working` — the status captured on the last non-`working` → `working` transition, and `done` for a first turn that had no earlier status to go back to. The cancelled prompt produced nothing, so the row should look as if it never landed: a reply aborted mid-question reverts to `Blocked`, and the user's real answer is then a `blocked → working` approval-cycle reply (no task boundary), so `original_prompt` survives. Gated by `detect_cancelled_turns`.

### Continuation prompts

Some replies look like new prompts but are really *"keep going with what you were doing"* — `"go"`, `"continue"`, `"proceed"` — or a bare approval like `"yes"` / `"y"` / `"ok"`. These bite when the row is genuinely `Done` / `Idle` with no pending question to revert to: the agent finishes without a `?` and without a permission or plan-approval dialog pending, or its closing question slips past the [question detector](classification), and the user replies `"y"`. From `Done` / `Idle` a one-word follow-up would otherwise look like a fresh task and clobber `original_prompt` plus reset the working timer — the recurring *"the row now shows `y` as the task"* bug. Treating approvals as continuations catches that. (The other route — a reply aborted mid-question — is handled by the revert-to-`Blocked` above; the two together cover both ways the row can leave `Blocked` before the real answer arrives.)

To avoid that, `apply_set` checks the incoming `label` against `Config::continuation_prompts` (defaults: `["go", "continue", "proceed", "yes", "y", "yeah", "yep", "yup", "ok", "okay", "sure", "go ahead", "do it"]`). If the trimmed label matches any phrase exactly (case-insensitive), the task boundary is suppressed:

- `original_prompt` stays pinned to the prior task.
- `working_accumulated_ms` is preserved (the timer continues from where it left off).
- `label` is still updated to the incoming text (e.g. `"go"`), but with `status = working` the row's text falls back to `original_prompt` anyway, so the user keeps seeing the real task on screen.

Match is **exact** after trim, not substring or starts-with — `"go"` matches `"go"` and `"Go"` and `" go "`, but not `"go ahead"` or `"google something"`. If you want phrases like `"go ahead"` to count, add them to the list verbatim.

This rule only fires on what would otherwise be a task boundary (transitions into `working` from `done` / `idle` / `working` / `waiting`). On a `blocked → working` transition the row is already in an approval cycle, so the rule is a no-op there.

## What the widget actually shows

`AgentSession::primary_text` (`src-tauri/src/state.rs`) chooses between the label and the task based on the row's current status. Telegram notifications show the same text. The row draws it through `AgentSession::row_line`, which the display snapshot stamps onto every row it sends the frontend. A terminal's own context line — agwinterm's on Windows, agterm's on macOS — is written from the task alone (`AgentSession::shown_task`), never the label, since the tab title's glyph already says the row is asking:

| Status                      | Widget shows                                          |
|---                          |---                                                    |
| `blocked`                  | `label` — the agent's question or permission request  |
| `error`                     | `label` — the error message                           |
| everything else             | the task if set, else `label`                         |

The task is `original_prompt`, except where another agent's message began it. Claude Code delivers that message as an ordinary prompt, so `original_prompt` holds the whole envelope, and the row shows instead what `src-tauri/src/prompt_origin.rs` settled when the prompt arrived: the sending agent's own task (`delegated_task`), or, where that could not be resolved, the message's first line (`message_line`). Both move with `original_prompt` through `label_policy::select`, so they describe the task it holds; `original_prompt` keeps the envelope for the History window. The row's hover tooltip lists its past tasks by the same rule, from `AgentSession::task_lines` rather than the raw dialog entries. Any envelope that still reaches a display, in `label`, in a past dialog entry or in an older row's `original_prompt`, is shown as an excerpt of its message, never as the envelope.

Where that text is empty, as on a row restored after `/clear` or a restart, `row_line` falls back to the most recent task in the row's history, and the row draws it muted so it reads as a past task.

The principle: when the agent is **blocked**, surface what's blocking it (the transient `label`). When the agent is **acting on or finished with a task**, surface the task itself (`original_prompt`).

## Walk-through

A typical task with one approval cycle and a clarifying question, then a brand-new task on the same row:

| Step | Hook fires                       | Status     | `label`                          | `original_prompt`                                | Widget shows                  |
|---   |---                               |---         |---                               |---                                               |---                            |
| 1    | UserPromptSubmit "fix foo.py"    | `working`  | `"fix foo.py"`                   | `"fix foo.py"` *(new row: captured)*                       | `"fix foo.py"`                |
| 2    | PermissionRequest                | `blocked` | `"needs approval: Bash"`         | `"fix foo.py"` *(pinned)*                        | `"needs approval: Bash"`      |
| 3    | UserPromptSubmit "y"             | `working`  | `"y"`                            | `"fix foo.py"` *(blocked → working: pinned)*    | `"fix foo.py"`                |
| 4    | Stop with question               | `blocked` | `"has a question"`               | `"fix foo.py"` *(pinned)*                        | `"has a question"`            |
| 5    | UserPromptSubmit follow-up       | `working`  | `"the follow-up text"`           | `"fix foo.py"` *(still pinned)*                  | `"fix foo.py"`                |
| 6    | Stop, task done                  | `done`     | `"the follow-up text"` *(preserved; Stop emits no label, so the prior `label` from step 5 stays)* | `"fix foo.py"` *(pinned)*           | `"fix foo.py"`                |
| 7    | UserPromptSubmit "add tests"     | `working`  | `"add tests"`                    | `"add tests"` *(done → working: re-captured)*    | `"add tests"`                 |

Step 7 is the only point after step 1 where `original_prompt` gets re-captured: the prior status was `done`, so the table's third row fires. Every other transition into `working` (steps 3 and 5) had `blocked` as the prior status, falling under "leave pinned."

## Implementation pointers

- The state machine is enforced by `src-tauri/src/state.rs::apply_set`, which delegates the `(label, original_prompt)` decision to `src-tauri/src/label_policy.rs::select`. Every `(label, original_prompt)` decision is made there, so the rules are applied in exactly one place; the other writers of session state (the watcher's promotion, the Esc-cancel revert, the WAIT backstop) change the status and never choose a label.
- A subagent's permission prompt doesn't go through `apply_set`: `AppState::open_subagent_prompt` shows the prompt's label over the row, and the release writes back the `label` the main agent's own events left, so the sticky fields underneath are never touched. Main-agent events that arrive meanwhile still run through `apply_set` and `select`, against that underlying state (`state::with_base`).
- The transcript watcher (`src-tauri/src/log_watcher.rs::apply_watcher_update`) is allowed to upgrade status to `working`, update model / token counts, and upsert the latest Assistant dialog text, but it cannot touch `label` or `original_prompt` — those stay hook-authoritative.
- See [Data flow](data-flow) for how `apply_set` fits into the full event pipeline, and [Classification](classification) for how the per-event `(status, label)` pair is computed before reaching the state layer.
