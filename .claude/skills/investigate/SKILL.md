---
name: investigate
description: Explain why a tracked dashboard agent/session is in its current state (WORK/WAIT/DONE/ERROR/IDLE) using the backend's permanent decision log — no transcript or source reading needed. TRIGGER when the user asks "why is <agent> in WAIT/WORK/...", "why does <agent> show <state>", "investigate <agent>", or wants the reason behind a session's status. Takes an agent name as input, or lists the current sessions to choose from when none is given.
---

# Investigate agent state

Explains why a tracked agent sits in its current dashboard state, using only the
permanent **decision log** the backend writes to `widget.jsonl`. Every
classification and state correction is logged there with a `decision` field and
a human `reason` — including the matched question-rule and a text snippet for
the question path — so an investigation never needs to open the agent's
transcript or read the Rust code.

## Data source

App data dir (`investigate.py` resolves this automatically):
- Windows: `%APPDATA%\com.anothersava.claude-code-dashboard`
- macOS: `~/Library/Application Support/com.anothersava.claude-code-dashboard`

Files read: `widget.jsonl` (the decision log) and `custom_names.json` (chat_id →
display name). The log is append-only and forward-looking — it explains state
changes that happened *after* the build that introduced decision logging, so a
just-deployed dashboard has a sparse log until events flow.

## Decision vocabulary

Each decision line carries `"decision":"<code>"`, the resolved agent (`chat_id`
or `id`), and a `reason`:

- `classify` — a hook event set the row's status (fields: `event`, `status`,
  `label`, `reason`). For a `Stop`/idle event the reason spells out the question
  verdict: `turn ended on a question [<rule>]: "<snippet>"` or `… is not a question: "<snippet>"`.
- `resume_working` — the transcript watcher saw new activity (a tool call or
  user turn) after a pause and promoted the row back to Working. This is the
  path that clears a stale WAIT once the user answers an `AskUserQuestion`.
- `correct_to_awaiting` / `correct_to_done` — the watcher re-judged the final
  assistant turn once it flushed to disk, fixing a verdict `Stop` made too early.
- `revert_cancelled` — an Esc-cancelled turn (no lifecycle hook) reverted to its
  pre-prompt status (`status` field = where it landed).
- `apply_set` — the state-machine transition (`prior_status` → `new_status`,
  `task_boundary`, `continuation_suppressed`).
- `session_clear` / `compact_boundary` — session removed / context-compaction
  separator inserted.

## Workflow

### 1. Pick the agent

- If the user named an agent, pass it straight to the script.
- If they didn't, run the script with no name to list current sessions, then use
  `AskUserQuestion` to let them choose (unless the context makes the target
  obvious).

The script matches the name against chat_ids, custom display names, and unique
substrings, so "travel", "travel-map", or a renamed "ai-dashboard" all resolve.

### 2. Read the decision trail

```bash
python3 .claude/skills/investigate/investigate.py            # list agents
python3 .claude/skills/investigate/investigate.py travel-map  # explain one
```

Run it from the repo root. The explain output gives the current state, the
decision that set it (with its reason), and the recent decision timeline.

### 3. Answer

Translate the trail into a plain-language answer:

- **Why it's in this state**: quote the `reason` of the setting decision. For a
  WAIT, that's almost always a `classify` with a question rule (the agent ended
  its turn asking something) or a tool gate (`AskUserQuestion` / permission
  dialog), or a `revert_cancelled` landing back on `Awaiting`.
- **Whether it's correct or stuck**: a WAIT whose newest decision is the
  question/gate that caused it is genuinely waiting on the user. A WAIT that the
  user has already answered should show a later `resume_working` or
  `correct_to_done`; if it doesn't, that's the bug to dig into.
- Keep it short. Lead with the state and the one-line reason; include the
  timeline only if it adds clarity. Don't dump raw JSON.

If the script reports no decisions for the agent (e.g. a fresh deploy, or the
agent has been idle since before decision logging landed), say so and fall back
to the live `widget.jsonl` tail or the transcript only if the user wants more.
