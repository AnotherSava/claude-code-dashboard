---
layout: default
title: Sticky labels
---

[Home](..) | [Claude Code](claude-code) | [HTTP API](http-api) | [Classification](classification) | [Sticky labels](sticky-labels) | [Data flow](data-flow) | [Development](development)

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

One detail relevant to the state machine: when the adapter emits `None` for the label, the state layer keeps the **prior** `label` value rather than blanking it (`src-tauri/src/label_policy.rs:49`). This is how `Stop` and `Notification` events that carry no label of their own (just a status change) leave the previous text in place.

## How `original_prompt` is set

The state layer (`src-tauri/src/state.rs::apply_set` via `src-tauri/src/label_policy.rs::select`) decides what happens to `original_prompt` on every `set` event. The decision depends on whether the session already exists, the prior status, and the new status:

| Existing session? | Prior status                      | New status | Action on `original_prompt`                                                  |
|---                |---                                |---         |---                                                                           |
| no                | —                                 | `working`  | set to incoming `label`                                                      |
| no                | —                                 | anything   | leave `None`                                                                 |
| yes               | `done` / `idle` / `working`       | `working`  | **re-capture** to incoming `label` (a new task is starting); also reset `working_accumulated_ms = 0` and `state_entered_at = now` — *unless the incoming `label` is a [continuation prompt](#continuation-prompts), in which case the boundary is suppressed and the row is treated as if it were an approval cycle* |
| yes               | `awaiting`                        | `working`  | leave pinned (approval cycle: agent asked, user answered)                    |
| yes               | any other                         | any        | leave pinned                                                                 |

The third row is the **task boundary**: a transition into `working` from any status *except* `awaiting` counts as a new task.

- `done` / `idle` → `working` is the natural case: the agent has finished (or is freshly seeded) and the user is starting something new.
- `working` → `working` covers the **cancellation case**: the user hit `Esc` mid-task and submitted a fresh prompt before the agent could emit a `Stop`. Without this rule the row would still display the cancelled prompt, which is misleading.
- `awaiting` → `working` is the only transition into `working` that's **not** a boundary. It's the canonical approval cycle (agent asks → user answers → agent resumes), so typing `y` doesn't clobber the original prompt.

If the new event has `label: None` on a task boundary, the prior `original_prompt` survives unchanged.

### Continuation prompts

Some replies look like new prompts but are really *"keep going with what you were doing"* — `"go"`, `"continue"`, `"proceed"`, etc. The agent often hits `Done` rather than `Awaiting` when its draft doesn't end in `?` (no `Notification` of type `permission_prompt` / `plan_approval` / `idle_prompt` either), and so a one-word follow-up would otherwise look like a fresh task and clobber `original_prompt` plus reset the working timer.

To avoid that, `apply_set` checks the incoming `label` against `Config::continuation_prompts` (defaults: `["go", "continue", "proceed"]`). If the trimmed label matches any phrase exactly (case-insensitive), the task boundary is suppressed:

- `original_prompt` stays pinned to the prior task.
- `working_accumulated_ms` is preserved (the timer continues from where it left off).
- `label` is still updated to the incoming text (e.g. `"go"`), but with `status = working` `displayLabel` falls back to `original_prompt` anyway, so the user keeps seeing the real task on screen.

Match is **exact** after trim, not substring or starts-with — `"go"` matches `"go"` and `"Go"` and `" go "`, but not `"go ahead"` or `"google something"`. If you want phrases like `"go ahead"` to count, add them to the list verbatim.

This rule only fires on what would otherwise be a task boundary (transitions into `working` from `done` / `idle` / `working`). On an `awaiting → working` transition the row is already in an approval cycle, so the rule is a no-op there.

## What the widget actually shows

The frontend's `displayLabel` (`src/lib/types.ts:58-61`) chooses between the two fields based on the row's current status:

| Status                      | Widget shows                                          |
|---                          |---                                                    |
| `awaiting`                  | `label` — the agent's question or permission request  |
| `error`                     | `label` — the error message                           |
| `working` / `done` / `idle` | `original_prompt` if set, else `label`                |

The principle: when the agent is **blocked**, surface what's blocking it (the transient `label`). When the agent is **acting on or finished with a task**, surface the task itself (`original_prompt`).

## Walk-through

A typical task with one approval cycle and a clarifying question, then a brand-new task on the same row:

| Step | Hook fires                       | Status     | `label`                          | `original_prompt`                                | Widget shows                  |
|---   |---                               |---         |---                               |---                                               |---                            |
| 1    | UserPromptSubmit "fix foo.py"    | `working`  | `"fix foo.py"`                   | `"fix foo.py"` *(idle → working: captured)*      | `"fix foo.py"`                |
| 2    | Notification permission          | `awaiting` | `"needs approval: Bash"`         | `"fix foo.py"` *(pinned)*                        | `"needs approval: Bash"`      |
| 3    | UserPromptSubmit "y"             | `working`  | `"y"`                            | `"fix foo.py"` *(awaiting → working: pinned)*    | `"fix foo.py"`                |
| 4    | Stop with question               | `awaiting` | `"has a question"`               | `"fix foo.py"` *(pinned)*                        | `"has a question"`            |
| 5    | UserPromptSubmit follow-up       | `working`  | `"the follow-up text"`           | `"fix foo.py"` *(still pinned)*                  | `"fix foo.py"`                |
| 6    | Stop, task done                  | `done`     | `"the follow-up text"` *(preserved; Stop emits no label, so the prior `label` from step 5 stays)* | `"fix foo.py"` *(pinned)*           | `"fix foo.py"`                |
| 7    | UserPromptSubmit "add tests"     | `working`  | `"add tests"`                    | `"add tests"` *(done → working: re-captured)*    | `"add tests"`                 |

Step 7 is the only point after step 1 where `original_prompt` gets re-captured: the prior status was `done`, so the table's third row fires. Every other transition into `working` (steps 3 and 5) had `awaiting` as the prior status, falling under "leave pinned."

## Implementation pointers

- The state machine is enforced by `src-tauri/src/state.rs::apply_set`, which delegates the `(label, original_prompt)` decision to `src-tauri/src/label_policy.rs::select`. Every mutation to session state — from HTTP events, the transcript watcher, or Tauri commands — funnels through `apply_set` so the rules are applied in exactly one place.
- The transcript watcher (`src-tauri/src/log_watcher.rs::apply_watcher_update`) is allowed to upgrade status to `working`, update model / token counts, and upsert the latest Assistant dialog text, but it cannot touch `label` or `original_prompt` — those stay hook-authoritative.
- See [Data flow](data-flow) for how `apply_set` fits into the full event pipeline, and [Classification](classification) for how the per-event `(status, label)` pair is computed before reaching the state layer.
