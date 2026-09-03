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
| `Notification`     | `blocked` (default) — `done` if `notification_type == "idle_prompt"` with no question | See the notification-type table below.                      |
| `PreToolUse`       | `blocked` for `AskUserQuestion` / `ExitPlanMode` only; other tools ignored         | Label: `"has a question"` for `AskUserQuestion`, `"plan approval"` for `ExitPlanMode`. The matcher in `~/.claude/settings.json` should restrict the hook to these two tools (see [Installation → Wire the Claude Code hook](../install#2-wire-the-claude-code-hook)) — Claude Code buffers the `tool_use` block until the user answers, so the JSONL transcript can't carry the signal in flight. |
| `Stop`             | `blocked` if the final assistant message ends on a question, else `waiting` if background work is still in flight, else `done` (see [detection rules](#question-detection)) | Settled here and not revisited: the payload carries the final text as `last_assistant_message` and any in-flight work as `background_tasks`, so `classify_stop` has everything it needs at `Stop` time. The question check ignores configured benign closers and openers. |
| `SessionEnd`       | emits `Clear` (removes the row, unless a live sibling owns it)                       | Bypasses status classification entirely.                       |

`SessionStart` and `Notification` share a code path because Claude Code occasionally emits notifications under either name; the dispatcher merges them.

`PostToolUse` is intentionally ignored. Once the user answers an `AskUserQuestion` / `ExitPlanMode`, the next `UserPromptSubmit` or the transcript watcher carries the row out of `blocked`.

### Notification subtypes

`Notification` further splits on `payload.notification_type`:

| `notification_type`  | Status                                                       | Label                                                |
|---                   |---                                                           |---                                                   |
| `permission_prompt`  | `blocked`                                                   | `"needs approval: <tool>"` — `<tool>` is the text after `"use "` in the message; falls back to `"tool"` if the marker is absent. |
| `plan_approval`      | `blocked`                                                   | `"plan approval"` (fixed)                            |
| `idle_prompt`        | `blocked` if transcript ends with `?` (non-benign), else `done` | `"has a question"` when flipped, else `None`     |
| anything else        | `blocked`                                                   | cleaned `payload.message`, truncated to 60 chars     |
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

## Question detection

`Stop` decides whether the agent is genuinely done or is handing back to the user, and it decides it from the payload rather than from the transcript: `last_assistant_message` carries the final assistant text — "avoids the need to read and parse the transcript file" — so `classify_stop` runs the question check on it directly and the row is settled there.

There is no re-read and no later correction. An earlier design classified from the *prior* turn's text, because `Stop` fires before the final turn flushes to JSONL, and had the transcript watcher fix the verdict both ways once the real text landed. Adopting `last_assistant_message` removed the reason for all of it: `flushed_turn_verdict`, `promote_done_to_blocked`, `demote_scanned_blocked_to_done` and the `status_from_transcript_scan` provenance flag are gone, and `Notification` (subtype `idle_prompt`) is ignored outright rather than acting as a second opinion. What the watcher still does is promote a paused row back to `working` once the transcript shows the main turn resumed — it never demotes (see [data flow](data-flow)).

One helper does the detection:

**`is_a_question(text, rules)`** — pure check on a string, four detection paths. The `rules` argument bundles two config-driven lists that always travel together: `benign_closers` (suffix-matched) and `benign_openers` (prefix-matched). Before any path runs, inline Markdown formatting characters (`*`, `_`, `` ` ``, `#`, `~`) are stripped so a final `**Push?**` reduces to `Push?` and is still recognized — only those marker characters are removed; newlines and every other character (crucially the terminal `?`) are preserved.

**Pre-step — peel trailing `Note:` paragraphs:** Before the four paths run, blank-line-separated paragraphs that open with `Note:` are dropped from the end, newest-first. Agents often append a housekeeping caveat *after* the closing question — `"Shall I proceed?\n\nNote: I also renamed X — flag if you'd rather keep it."` — and every path keys off the tail of the text, so the note would otherwise mask the question and settle the turn `done`. A note that is *itself* a hand-back (it registers as a question on its own) is kept, so peeling can only ever turn `done → blocked`, never hide an ask that lives only inside a note. The verdict runs on the peeled text; the decision-log snippet is peeled too, so the log quotes the real question, not the note.

**Path 1 — trailing `?`:**

1. If `text` (after trim) ends with `)`, peel off one trailing `(...)` group **only when** the substring before the matching `(` ends with `?`. This handles option lists like `"Save these? (all / numbers / none)"` → `"Save these?"`. Other trailing parens (e.g. `"Look at this code (foo.py)"`) are left alone — there's no `?` before them, so the text falls through unchanged.
2. After that strip, if the text ends with `?`, two filters can skip this path. First, `Config::benign_closers` — case-insensitive suffix match (defaults `"What's next?"`, `"or are you good?"`, and `"or leave it?"`); a hit skips. Second, `Config::benign_openers` — case-insensitive prefix match against the **final sentence** (default `"anything"`); a sign-off like `"Anything you'd like to look at?"` opens with a benign offer word and so skips. Both exist because Claude often signs off with a polite question that isn't a real ask — flipping to `blocked` on every `What's next?` or `or are you good?` would be noise.

   The two filters differ on what they excuse downstream. A benign **opener** skips only Path 1; the final sentence is still scanned by Paths 2–4, so an embedded real ask is caught by the permission-seeking path and `"Anything else, or shall I commit?"` stays `blocked`. A benign **closer**, by contrast, marks its whole closing sentence an optional sign-off — that sentence is dropped before Paths 2–4 run, so a permission-seeking phrase *inside* it (`"Want me to drive a browser check, or are you good?"`) reads as part of the offer and stays `done`. A genuine ask in an *earlier* sentence still awaits (`"Should I delete the backup first? Or are you good?"` stays `blocked`).

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

Failure modes are silent: a `Stop` payload carrying no `last_assistant_message`, or an empty one, is treated as "no question" and settles the row on the background-work check alone. The adapter never crashes a status update over a missing or unreadable field.

## Decision log

Every status-affecting decision is written to `widget.jsonl` (the same tracing sink `logging.rs` owns) as a structured line carrying a stable `decision` field and a human `reason`, keyed by the resolved `chat_id`. The reason for a question verdict names which detection path fired and quotes a snippet of the assistant text, so "why is this row `blocked`?" is answerable from the log alone — no transcript or source reading.

| `decision`                            | Emitted from              | Meaning                                                                                                                                                                      |
|---                                    |---                        |---                                                                                                                                                                          |
| `classify`                            | `http_server` (`event -> set`) | A hook event set the row's status. For `Stop` / idle prompts the `reason` reads `<kind> on a question [<rule>]: "<snippet>"` or `<kind>; final message is not a question: "<snippet>"`, where `<kind>` is `turn ended` or `idle prompt`. |
| `resume_working`                      | `log_watcher`             | The transcript watcher saw new activity (a tool call or user turn) after a pause and promoted the row back to `working` — the path that clears a stale `blocked` once the user answers an `AskUserQuestion`. |
| `settle_waiting`                      | `waiting_settle`          | A row sat in `waiting` (light-blue WAIT) unchanged past `waiting_settle_ms` and was settled to `done` — the backstop for a background shell task the user killed, which ends silently and so fires no follow-up `Stop`. Carries `waited_ms` / `window_ms`. |
| `revert_cancelled`                    | `log_watcher`             | An Esc-cancelled turn (no lifecycle hook) reverted to its pre-prompt status — the `status` field records where it landed.                                              |
| `apply_set`                           | `state.rs`                | The state-machine transition: `prior_status` → `new_status`, plus `task_boundary` and `continuation_suppressed`.                                                            |
| `session_clear` / `compact_boundary`  | `http_server`             | Session removed / context-compaction history separator inserted.                                                                                                            |

The project-local `investigate` skill (`.claude/skills/investigate/investigate.py`) reads these lines to reconstruct an agent's current state and its decision chain: `investigate.py <agent>` explains one session; no argument lists the active sessions to choose from.

## What this layer does *not* decide

- **Whether the user-visible label changes.** The adapter emits a candidate `(status, label)` pair; the [sticky-label state machine](sticky-labels) decides whether the row's `original_prompt` updates, gets re-captured at a task boundary, or stays pinned across an approval cycle.
- **Token counts and model.** Those come from the transcript watcher (`log_watcher.rs`) reading assistant turn metadata, not from hook events.
- **Timer accumulation.** `state.rs::apply_set` owns the `working_accumulated_ms` arithmetic on status transitions.
