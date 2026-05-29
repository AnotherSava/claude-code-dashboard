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
| `cwd` is missing or whitespace-only               | `claude-<session_id[:8]>` (or `claude-unknown`)  |

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
| `PreToolUse`       | `awaiting` for `AskUserQuestion` / `ExitPlanMode` only; other tools ignored         | Label: `"has a question"` for `AskUserQuestion`, `"plan approval"` for `ExitPlanMode`. The matcher in `~/.claude/settings.json` should restrict the hook to these two tools (see [Claude Code setup](claude-code#setup)) — Claude Code buffers the `tool_use` block until the user answers, so the JSONL transcript can't carry the signal in flight. |
| `Stop`             | `done` — flips to `awaiting` if last assistant turn contains a question (see [detection rules](#transcript-question-detection)) | Question check ignores configured benign closers.              |
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

`Stop` and `Notification` (subtype `idle_prompt`) need to decide whether the agent is genuinely done or is actually waiting for an answer. The flow has two helpers:

**`last_assistant_text(path)`** — walks the JSONL transcript at `payload.transcript_path`:

1. Read the file line-by-line.
2. For each line, parse as JSON. Skip malformed lines.
3. Skip entries whose `message.role` isn't `"assistant"`.
4. Extract assistant text from `message.content`:
   - if it's a JSON string, take the trimmed value;
   - if it's an array, walk each block and take the trimmed `text` from blocks where `type == "text"`.
5. Track the last non-empty text seen (so trailing whitespace-only assistant turns don't reset the state) and return it.

**`is_a_question(text, benign_closers)`** — pure check on a string, three detection paths:

**Path 1 — trailing `?`:**

1. If `text` (after trim) ends with `)`, peel off one trailing `(...)` group **only when** the substring before the matching `(` ends with `?`. This handles option lists like `"Save these? (all / numbers / none)"` → `"Save these?"`. Other trailing parens (e.g. `"Look at this code (foo.py)"`) are left alone — there's no `?` before them, so the text falls through unchanged.
2. After that strip, if the text ends with `?`, check against `Config::benign_closers` — case-insensitive suffix match. A hit skips this path. Defaults: `"What's next?"`, `"Anything else?"`. They exist because Claude often signs off with a polite question that isn't a real ask — flipping to `awaiting` on every `What's next?` would be noise.

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

This catches questions embedded mid-paragraph like `"Want me to add that? The plan: write sessions.json to disk."` where the response continues past the `?`. The phrase list is empirically derived from real assistant messages — only patterns that actually appeared are included; new ones are added as observed. `"save this?"` / `"save these?"` were added for the `/reflect` and `/commit` save prompts, whose `"Save this? (all / 1 / none)"` menu can be trailed by a clause like `"— then I'll run /commit."` that defeats path 1 (the text no longer ends with `?`); the baked-in `?` keeps a declarative `"save this config"` from matching. `"can you"` / `"could you"` / `"did you"` / `"want to"` catch directed second-person questions whose paragraph continues past the `?` (`"Did you try the admin launch? That's the most likely fix."`). Only the **last** paragraph is scanned: a question in an earlier paragraph followed by a concluding statement (e.g. `"Want me to fix it?\n\nI went ahead and fixed it."`) correctly returns `false`.

Only round brackets `()` are recognized for the option-list strip; `[]` and `{}` aren't peeled.

**Path 3 — `Paste …` request in last paragraph:**

If neither path above matches, check whether any sentence in the last paragraph (split on `.!?` and newlines) starts with `"paste "` (case-insensitive). This catches the imperative hand-back where the agent waits for the user to paste output but never ends on a `?` — `"Paste the tableinfos output and I'll finish arena."`, `"Paste whatever it prints."`. Only a **sentence-initial** `Paste` counts, so a mid-sentence mention like `"you can paste this"` or `"I'll paste the result"` doesn't trigger.

Failure modes are silent: a missing transcript file returns `None` from `last_assistant_text` (treated as "no question"), and malformed JSONL lines are individually skipped. The adapter never crashes a status update because of a transcript read error.

## What this layer does *not* decide

- **Whether the user-visible label changes.** The adapter emits a candidate `(status, label)` pair; the [sticky-label state machine](sticky-labels) decides whether the row's `original_prompt` updates, gets re-captured at a task boundary, or stays pinned across an approval cycle.
- **Token counts and model.** Those come from the transcript watcher (`log_watcher.rs`) reading assistant turn metadata, not from hook events.
- **Timer accumulation.** `state.rs::apply_set` owns the `working_accumulated_ms` arithmetic on status transitions.
