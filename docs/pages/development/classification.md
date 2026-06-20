---
layout: default
title: Classification
parent: Development
nav_order: 1
---

How the Claude adapter turns a raw lifecycle payload into the `(chat_id, status, label)` tuple the widget renders. All logic in this page lives in `src-tauri/src/adapters/claude.rs`; the Python hook (`integrations/claude_hook.py`) is a pure transport layer and does no classification.

A single event payload flows through four independent steps in this order: chat-id derivation, event-to-status classification, label formatting, and (for `Stop` / `Notification`) transcript question detection. The label policy that decides what's actually shown on screen is downstream — see [Sticky labels](sticky-labels) for the rules that pin the original prompt across approval cycles.

## Chat-id derivation

Each Claude Code session collapses to one row in the widget. The row's `id` (a.k.a. `chat_id`) is derived from `payload.cwd` and the configured `projects_root` in `config.json`:

| Input                                             | Resulting `chat_id`                              |
|---                                                |---                                               |
| `cwd` is under `projects_root` (case-insensitive) | relative path; `/`, `-`, `_` replaced with spaces|
| `cwd` is outside `projects_root` (or root unset)  | basename of `cwd`                                |
| `cwd` exactly equals `projects_root`              | basename of `projects_root`                      |
| `cwd` is missing or whitespace-only               | `claude-unknown` (defensive; payloads always carry `cwd`) |

Backslashes are normalized to forward slashes before matching, so Windows paths work uniformly. Trailing slashes on `cwd` are tolerated. Examples (with `projects_root = "d:/projects"`):

| `cwd`                              | `chat_id`           |
|---                                 |---                  |
| `D:/projects/bga/assistant`        | `bga assistant`     |
| `d:/projects/foo-bar/sub_dir/leaf` | `foo bar sub dir leaf` |
| `D:\projects\sub\deep`             | `sub deep`          |
| `d:/projects`                      | `projects`          |
| `c:/Users/foo/bar`                 | `bar`               |

This derivation runs per event, but the result is only the **first-seen anchor**: `http_server` locks each `session_id` to the `chat_id` derived on its first event (`chat_id_registry`), so a mid-session `cd` into a subdirectory reuses the original id instead of spawning a second row. `/clear` mints a new `session_id` with the same `cwd`, so it re-derives — and re-locks — the same id.

## Event → status

The adapter recognizes six event names. Anything else returns `Ignore` and the widget state is untouched.

| Event              | Status produced                                                                     | Notes                                                          |
|---                 |---                                                                                  |---                                                             |
| `SessionStart`     | `idle` (no fields) — otherwise treated like `Notification`                          | Used to seed an empty row before any user activity.            |
| `UserPromptSubmit` | `working`                                                                           | Label is the cleaned prompt; blank prompt → label `None`.      |
| `Notification`     | `awaiting` (default) — `done` if `notification_type == "idle_prompt"` with no question | See the notification-type table below.                      |
| `PreToolUse`       | `awaiting` for `AskUserQuestion` / `ExitPlanMode` only; other tools ignored         | Label: `"has a question"` for `AskUserQuestion`, `"plan approval"` for `ExitPlanMode`. The matcher in `~/.claude/settings.json` should restrict the hook to these two tools (see [Installation → Wire the Claude Code hook](../install#2-wire-the-claude-code-hook)) — Claude Code buffers the `tool_use` block until the user answers, so the JSONL transcript can't carry the signal in flight. |
| `Stop`             | `done` — flips to `awaiting` if last assistant turn contains a question (see [detection rules](#transcript-question-detection)) | Question check ignores configured benign closers. `Stop` fires *before* the final assistant turn flushes to JSONL, so it classifies from the **prior** turn's text and can be wrong either way — missing a trailing question (→ wrong `done`) or, when a statement turn follows a question turn, reading the stale question (→ wrong `awaiting`). The transcript watcher corrects both once the real text lands (see [data flow](data-flow)). |
| `SessionEnd`       | emits `Clear` (removes the row)                                                     | Bypasses status classification entirely.                       |

`SessionStart` and `Notification` share a code path because Claude Code occasionally emits notifications under either name; the dispatcher merges them.

`PostToolUse` is intentionally ignored. Once the user answers an `AskUserQuestion` / `ExitPlanMode`, the next `UserPromptSubmit` or the transcript watcher carries the row out of `awaiting`.

### Notification subtypes

`Notification` further splits on `payload.notification_type`:

| `notification_type`  | Status                                                       | Label                                                |
|---                   |---                                                           |---                                                   |
| `permission_prompt`  | `awaiting`                                                   | `"needs approval: <tool>"` — `<tool>` is the text after `"use "` in the message; falls back to `"tool"` if the marker is absent. |
| `plan_approval`      | `awaiting`                                                   | `"plan approval"` (fixed)                            |
| `idle_prompt`        | `awaiting` if transcript ends with `?` (non-benign), else `done` | `"has a question"` when flipped, else `None`     |
| anything else        | `awaiting`                                                   | cleaned `payload.message`, truncated to 60 chars     |
| empty type, empty message | `idle`                                                  | `None`                                               |

The 60-char truncation counts **characters, not bytes**, so multi-byte glyphs (emoji, CJK) are never split mid-codepoint.

## Prompt and label cleaning

User-visible text comes from `payload.prompt` (UserPromptSubmit) or `payload.message` (Notification). Both go through `clean_prompt`, which:

1. Replaces these whitespace characters with a single space: `\n`, `\r`, `\t`, vertical tab, form feed.
2. Replaces all characters in U+2300–U+23FF (Miscellaneous Technical, e.g. `⎿`) with a space.
3. Replaces all characters in U+2500–U+259F (Box Drawing + Block Elements, e.g. `│ ▌`) with a space.
4. Collapses runs of spaces into one and trims.

This cleaning applies to the **label** (the one-line preview shown in the dashboard row). The dialog entry persisted for the multi-line history view takes `payload.prompt` raw on UserPromptSubmit instead, so the history preserves newlines and the user's original formatting.

Other Unicode passes through untouched — accents, emoji, CJK, math symbols. The U+2300/U+2500 ranges are stripped because Claude Code's terminal output frequently leaks box-drawing glyphs into prompt and notification text.

## Transcript question detection

`Stop` and `Notification` (subtype `idle_prompt`) need to decide whether the agent is genuinely done or is actually waiting for an answer. The transcript watcher (`log_watcher.rs`) is a third caller: it reuses `is_a_question` to re-judge the verdict once the final assistant turn flushes to JSONL — the case `Stop` fires too early to read — and corrects the row **both ways** (`done → awaiting` for a missed question, `awaiting → done` for a stale-read one). The watcher's demote is gated on a provenance flag (`status_from_transcript_scan`) so it only overturns `awaiting` rows that came from this scan, never a tool-gating `awaiting` (see [data flow](data-flow)). The flow has two helpers:

**`last_assistant_text(path)`** — walks the JSONL transcript at `payload.transcript_path`:

1. Read the file line-by-line.
2. For each line, parse as JSON. Skip malformed lines.
3. Skip entries whose `message.role` isn't `"assistant"`.
4. Extract assistant text from `message.content`:
   - if it's a JSON string, take the trimmed value;
   - if it's an array, walk each block and take the trimmed `text` from blocks where `type == "text"`.
5. Track the last non-empty text seen (so trailing whitespace-only assistant turns don't reset the state) and return it.

**`is_a_question(text, rules)`** — pure check on a string, four detection paths. The `rules` argument bundles two config-driven lists that always travel together: `benign_closers` (suffix-matched) and `benign_openers` (prefix-matched). Before any path runs, inline Markdown formatting characters (`*`, `_`, `` ` ``, `#`, `~`) are stripped so a final `**Push?**` reduces to `Push?` and is still recognized — only those marker characters are removed; newlines and every other character (crucially the terminal `?`) are preserved.

**Path 1 — trailing `?`:**

1. If `text` (after trim) ends with `)`, peel off one trailing `(...)` group **only when** the substring before the matching `(` ends with `?`. This handles option lists like `"Save these? (all / numbers / none)"` → `"Save these?"`. Other trailing parens (e.g. `"Look at this code (foo.py)"`) are left alone — there's no `?` before them, so the text falls through unchanged.
2. After that strip, if the text ends with `?`, two filters can skip this path. First, `Config::benign_closers` — case-insensitive suffix match (default `"What's next?"`); a hit skips. Second, `Config::benign_openers` — case-insensitive prefix match against the **final sentence** (default `"anything"`); a sign-off like `"Anything you'd like to look at?"` opens with a benign offer word and so skips. Both exist because Claude often signs off with a polite question that isn't a real ask — flipping to `awaiting` on every `What's next?` or `Anything else?` would be noise. An embedded real ask isn't lost: it's still caught downstream by the permission-seeking path (Path 2), so `"Anything else, or shall I commit?"` stays `awaiting`.

**Path 2 — hand-back phrase in last paragraph:**

If path 1 doesn't match, extract the last paragraph of `text` (split by `\n\n`) and check whether it contains any of these phrases (case-insensitive). A phrase that already ends in `?` matches literally; the rest only count when a `?` follows them later in the same paragraph:

- `"want me to"`
- `"shall i"`
- `"should i"`
- `"do you want"`
- `"save this?"`
- `"save these?"`
- `"can you"`
- `"could you"`
- `"did you"`
- `"want to"`
- `"confirm "`
- `"ready to "`

This catches questions embedded mid-paragraph like `"Want me to add that? The plan: write sessions.json to disk."` where the response continues past the `?`. The phrase list is empirically derived from real assistant messages — only patterns that actually appeared are included; new ones are added as observed. `"save this?"` / `"save these?"` were added for the `/reflect` and `/commit` save prompts, whose `"Save this? (all / 1 / none)"` menu can be trailed by a clause like `"— then I'll run /commit."` that defeats path 1 (the text no longer ends with `?`); the baked-in `?` keeps a declarative `"save this config"` from matching. `"can you"` / `"could you"` / `"did you"` / `"want to"` catch directed second-person questions whose paragraph continues past the `?` (`"Did you try the admin launch? That's the most likely fix."`). `"confirm "` / `"ready to "` carry a trailing space so `confirmed` / `confirmation` don't match — they catch approval prompts whose `?` isn't last (`"Confirm v0.5.0 and these notes? On approval I'll …"`, `"Ready to tag v0.5.1 and push it? Reply with y …"`). Only the **last** paragraph is scanned: a question in an earlier paragraph followed by a concluding statement (e.g. `"Want me to fix it?\n\nI went ahead and fixed it."`) correctly returns `false`.

Only round brackets `()` are recognized for the option-list strip; `[]` and `{}` aren't peeled.

**Path 3 — hand-back request in last paragraph:**

If neither path above matches, check whether any sentence in the last paragraph (split on `.!?` and newlines) starts with one of the hand-back openers `"paste "`, `"please provide "`, or `"confirm "` (case-insensitive). This catches the imperative hand-back where the agent waits for the user to supply something but never ends on a `?` — `"Paste the tableinfos output and I'll finish arena."`, `"Please provide the model group and the model name."`, `"Confirm to tag v1.2.0, or request edits."`. Only a **sentence-initial** opener counts, so a mid-sentence mention like `"you can paste this"` or `"I'll paste the result"` doesn't trigger. The list is kept narrow and phrase-matched — a blanket `"please "` would misfire on informational openers like *"Please note …"* / *"Please see …"*, and the trailing space in `"confirm "` keeps `Confirmed: …` statements out.

**Path 4 — leading question in last paragraph:**

If nothing above matches, check whether the **first sentence** of the last paragraph is itself a question — it ends with `?` before a concluding clause follows. This catches a hand-back whose question leads and is then trailed by context, like `"Apply this edit? (yes / no) Everything else is aligned."` — the trailing sentence defeats path 1 (the whole text no longer ends with `?`), the option menu sits mid-text rather than at the end, and `"apply"` is no path-2 phrase, so without this nothing sees it. Three guards keep it tight:

1. The terminating `?` must immediately follow an **alphanumeric** character, so a bare mention of the glyph — `"a `` ` ``?`` ` `` immediately followed by …"`, which markdown-stripping leaves as `"a ? …"` — isn't read as a sentence terminator.
2. The first sentence must **not** open with a self-directed phrase (`"let me "`, `"let's "`, `"lets "`, `"i'll "`, `"i will "`, `"i'm going to "`, `"i am going to "`). These mark the agent reasoning aloud and about to act, not asking — `"Let me investigate — does X have a cleaner fix? This affects what we do next."` stays `done`.
3. Configured benign closers (suffix) and benign openers (prefix) are both honored, so a leading polite `"What's next? …"` or a leading offer `"Anything you'd like to look at? …"` still doesn't flag.

This path was validated against the recorded dialog history (`prompt_history.json`): it fires on 12 of 60 real assistant turns with zero false positives. Like path 2 it scans only the **last** paragraph, and like it the question must be the paragraph's *first* sentence — a statement-first paragraph (`"The migration is ready. Looks good to you?"`) is left to path 1's trailing-`?` check.

Failure modes are silent: a missing transcript file returns `None` from `last_assistant_text` (treated as "no question"), and malformed JSONL lines are individually skipped. The adapter never crashes a status update because of a transcript read error.

## Decision log

Every status-affecting decision is written to `widget.jsonl` (the same tracing sink `logging.rs` owns) as a structured line carrying a stable `decision` field and a human `reason`, keyed by the resolved `chat_id`. The reason for a question verdict names which detection path fired and quotes a snippet of the assistant text, so "why is this row `awaiting`?" is answerable from the log alone — no transcript or source reading.

| `decision`                            | Emitted from              | Meaning                                                                                                                                                                      |
|---                                    |---                        |---                                                                                                                                                                          |
| `classify`                            | `http_server` (`event -> set`) | A hook event set the row's status. For `Stop` / idle prompts the `reason` reads `<kind> on a question [<rule>]: "<snippet>"` or `<kind>; final message is not a question: "<snippet>"`, where `<kind>` is `turn ended` or `idle prompt`. |
| `resume_working`                      | `log_watcher`             | The transcript watcher saw new activity (a tool call or user turn) after a pause and promoted the row back to `working` — the path that clears a stale `awaiting` once the user answers an `AskUserQuestion`. |
| `correct_to_awaiting` / `correct_to_done` | `log_watcher`         | The watcher re-judged the final assistant turn once it flushed, fixing a verdict `Stop` made too early (see [transcript question detection](#transcript-question-detection)). |
| `revert_cancelled`                    | `log_watcher` / `idle_probe` | An Esc-cancelled turn (no lifecycle hook) reverted to its pre-prompt status — the `status` field records where it landed.                                              |
| `apply_set`                           | `state.rs`                | The state-machine transition: `prior_status` → `new_status`, plus `task_boundary` and `continuation_suppressed`.                                                            |
| `session_clear` / `compact_boundary`  | `http_server`             | Session removed / context-compaction history separator inserted.                                                                                                            |

The project-local `investigate` skill (`.claude/skills/investigate/investigate.py`) reads these lines to reconstruct an agent's current state and its decision chain: `investigate.py <agent>` explains one session; no argument lists the active sessions to choose from.

## What this layer does *not* decide

- **Whether the user-visible label changes.** The adapter emits a candidate `(status, label)` pair; the [sticky-label state machine](sticky-labels) decides whether the row's `original_prompt` updates, gets re-captured at a task boundary, or stays pinned across an approval cycle.
- **Token counts and model.** Those come from the transcript watcher (`log_watcher.rs`) reading assistant turn metadata, not from hook events.
- **Timer accumulation.** `state.rs::apply_set` owns the `working_accumulated_ms` arithmetic on status transitions.
