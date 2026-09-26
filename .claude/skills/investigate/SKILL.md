---
name: investigate
description: Explain why a tracked dashboard agent/session is in its current state (WORK/WAIT/BLOCK/DONE/ERROR/IDLE) using the backend's permanent decision log — no transcript or source reading needed. TRIGGER when the user asks "why is <agent> in WAIT/WORK/BLOCK/...", "why does <agent> show <state>", "investigate <agent>", or wants the reason behind a session's status. Takes an agent name as input, or lists the current sessions to choose from when none is given.
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
  `label`, `reason`, `agent_id`). For a `Stop` the reason spells out the question
  verdict: `turn ended on a question [<rule>]: "<snippet>"` or `… is not a question: "<snippet>"`;
  a `Stop` with background work still running lands on WAIT. `agent_id` names
  the subagent on a `PermissionRequest` (and `PreToolUse`) a subagent raised, and
  is `None` on the main agent's. It proves nothing on other events: a subagent's
  `Notification`, `Elicitation`/`ElicitationResult` and `PreCompact` carry no
  `agent_id` either, so they read exactly like the main agent's own.
- `resume_working` — the transcript watcher saw new activity (a tool call or
  user turn) after a pause and promoted the row back to Working. This is the
  path that clears a stale BLOCK once the user answers an `AskUserQuestion`.
- `revert_cancelled` — an Esc-cancelled turn (no lifecycle hook) reverted to its
  pre-prompt status (`status` field = where it landed).
- `apply_set` — the state-machine transition (`prior_status` → `new_status`,
  `task_boundary`, `continuation_suppressed`).
- `gated: true` on `apply_set`, `resume_working` or `revert_cancelled` — a
  subagent's permission prompt was open on the row. The statuses on the line
  are the main agent's own, underneath; the row itself kept showing BLOCK.
- `subagent_prompt_open` — a subagent's `PermissionRequest` put the row on
  BLOCK over the main agent's state (`request`, `agent_id`, `agent_type`,
  `tool`, `pending`, `base_status`).
- `subagent_prompt_settled` — one or more of those prompts closed. `via` is
  `tool_result` (the agent's transcript recorded the call's result; also
  carries `is_error`, `matched_on`), `subagent_stop` or `stop_no_background`.
  `status` is what the row shows afterwards; `released: false` means other
  prompts still hold the BLOCK.
- `subagent_prompt_unmatched` — diagnosis only, once per prompt: the gate could
  not find the gated call (`outcome`: `no_transcript_path` / `no_transcript` /
  `no_tool_use`). The prompt stays open and the status does not move. Only
  `no_transcript_path` is final; after the other two the tick keeps looking and
  can still settle the prompt `via=tool_result`.
- `session_clear` / `compact_boundary` — session removed / context-compaction
  separator inserted.
- `settle_waiting` — the backstop settled a WAIT held by a background shell
  task to Done once it sat unchanged past the window (`waited_ms`,
  `window_ms`): a task the user killed ends silently, so no later `Stop` comes.
- `restore_row` — after a dashboard restart, a live session with no row got one
  back, its `status` read from the tab title the dashboard last wrote.
- `reap_exited` — the liveness reaper removed the row because its owning Claude
  process exited without a `SessionEnd` (e.g. you typed `exit` / closed the
  terminal). Carries the dead `pid` and the `prior_status` the row last held.
  This is a terminal decision: if it's the newest line, the row is gone on
  purpose, not stuck.

Historical, only in logs written by older builds: `enter_waiting` (WAIT now
comes from `classify` of a `Stop`), and `correct_to_blocked` /
`correct_to_done` (the watcher's re-judging of a too-early `Stop`, removed
once `Stop` carried its final message).

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
  BLOCK, that's almost always a `classify` with a question rule (the agent ended
  its turn asking something) or a tool gate (`AskUserQuestion` / permission
  dialog), a `subagent_prompt_open` (a subagent's permission dialog), or a
  `revert_cancelled` landing back on `Blocked`. A WAIT is a `classify` of a
  `Stop` whose background work was still running after the turn settled.
- **Whether it's correct or stuck**: a BLOCK whose newest decision is the
  question/gate that caused it is genuinely waiting on the user. A BLOCK that the
  user has already answered should show a later `resume_working` or a later
  `classify`; if it doesn't, that's the bug to dig into. A BLOCK from
  `subagent_prompt_open` clears on a `subagent_prompt_settled` with
  `released: true`. If none has followed, look for a `subagent_prompt_unmatched`
  line before concluding the user owes an answer. With `no_transcript_path` the
  prompt waits on `SubagentStop`, a `Stop` from the same session with no
  background work, or the row's removal; with the other outcomes the tick is
  still reading the agent's transcript and a late result settles it. After a
  release the script credits the status to the line that set it underneath and
  prints the release as `Revealed by`.
- Keep it short. Lead with the state and the one-line reason; include the
  timeline only if it adds clarity. Don't dump raw JSON.

If the script reports no decisions for the agent (e.g. a fresh deploy, or the
agent has been idle since before decision logging landed), say so and fall back
to the live `widget.jsonl` tail or the transcript only if the user wants more.
