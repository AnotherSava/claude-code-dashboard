---
created: 2026-09-24 14:17:30
platform: macos
---

# Make idle_awake and waiting_settle agree about a workflow-held WAIT, with one field rather than a liveness clock

Two modules read the same row and reach opposite conclusions about it. `waiting_settle` refuses to time-settle a WAIT held by background work that is not a killable shell task, because nothing distinguishes a running subagent from a finished one. `idle_awake` applies its silence bound to exactly those rows and drops the power assertion. The row is the same, the uncertainty is the same, and only one of the two treats it as a reason to hold still.

This memo replaces an earlier one proposing a per-session liveness clock fed from `token_scan`. That diagnosis was right and its two payoffs are both refuted — see the last section, which also keeps the one finding worth carrying forward.

## What fires

Measured 2026-09-26 on AIR, the only two times the silence bound has fired since `idle_awake` shipped on 2026-09-24:

- A 36m47s hold released at 02:40:34 PDT while workflow `wf_76f8fa3f-c1c` was still running. Work continued for 3m13s after the release.
- A 36m04s hold released at 05:21:34 PDT while `wf_cc4f53e8-051` was still running. Work continued for 27m40s.

Both rows were `Waiting`, and both carried a `classify` line reading `backstop disarmed (no killable shell task)` — so the adapter already knew a workflow was holding the row, and `idle_awake` never asked.

Workflow duration is what makes this recur rather than being a one-off: across 101 runs under `~/.claude/projects/*/*/subagents/workflows/` between 2026-08-04 and 2026-09-26, p50 is 20.8 min, p75 35.6 and p90 61.4, with 32% over 30 minutes. The window sits between the median and the third quartile of the thing it fires on, so no value separates a live workflow from a wedged one.

## What it costs, which is nothing

Claude Code's own sleep inhibitor covers a running workflow by construction. It is a refcount held while the session status is `busy`, and `busy` includes a delegated background task — read out of the 2.1.251 binary as `status:S.isLoading||S.delegatedActive?"busy":"idle"`, with the renewal loop guarded by `if(this.refCount>0||this.pendingKillTimeout!==null)`. Neither release could have caused a sleep, and on 2026-09-26 the display was additionally pinned on all night by an unrelated application, which held powerd's own assertion throughout both windows.

The honest value of the whole feature, over the seven days `pmset -g log` reaches back: `idle_awake` held 2h56m38s, of which **49 seconds** was coverage Claude Code's inhibitor was not already providing — two stretches of 25s and 24s, both at turn boundaries where the inhibitor's 30-second release grace explains them. No mid-turn lapse appears anywhere in that reach. The one lapse that did end in an `Idle Sleep` (2026-09-24 10:04:15, `ClientDied … [System: No Assertions]` with the sleep five seconds later) predates `idle_awake`'s first run by three hours.

Reconstruct those intervals by subtracting each assertion line's held duration from its timestamp. A `ClientDied … 00:03:59` reports the span *ending* at that moment, so consecutive four-minute renewals are contiguous; reading each one as the start of a gap invents lapses that are not there, which is how a first pass at this produced a 3.9-minute lapse that does not exist.

## The change

Replace `AgentSession::waiting_backstop_armed: bool` with `waiting_hold: Option<WaitingHold>`, where `WaitingHold` is `Shell | Managed`. `adapters::claude::classify_stop` already tallies `background_tasks` by task type and derives the flag from whether a `shell` entry is present, so it can emit `Some(Shell)` there and `Some(Managed)` when the tally is non-empty without one. Carry it through `Classification`, `SetInput`, `BaseState` and `AgentSession`. Then gate `waiting_settle` on `Some(Shell)`, which leaves its behaviour exactly as it is, and exempt `Some(Managed)` from `idle_awake`'s silence bound.

Add a base accessor beside `AgentSession::base_status` before writing that exemption. `state::show_gate` zeroes `waiting_backstop_armed` while `base_status` reads through the subagent gate, so a predicate combining the raw field with `base_status()` mixes two layers.

**The `None` arm is what makes this safe, and a bare `!armed` exemption is not.** That field is `#[serde(skip)]`, so it defaults to `false` after a restart, and `session_restore` constructs its rows with `false` outright while keeping `Waiting` wherever the registry reports the session busy. The flag means "no killable shell task" and never "a workflow is running" — so an exemption keyed on its absence would hold the Mac awake indefinitely on a restored row nothing vouches for. A positive discriminator cannot make that mistake.

## What it does not fix

A single long tool call stays invisible. Nothing is written to any transcript, main or subagent, between the entry issuing a `tool_use` and the entry consuming its result, so no transcript-derived clock reaches it and a `Working` row on a long build is still released at 30 minutes. Headroom is about ten minutes: the longest non-user-gating tool call measured over 30 days is 19.6 minutes.

The only signal that would reach it is a positive in-flight-tool predicate — an assistant entry whose trailing content block is a `tool_use` with no matching `tool_result` yet, which `log_watcher` already parses for `infer_state`. Recorded here so it is not re-derived; not worth building, since no release has ever landed on one.

## Why not the liveness clock

The superseded proposal was a per-session "last seen alive" stamp fed from `token_scan`'s 60s walk, which already covers `subagents/`. Both payoffs it claimed fail against measurement. `idle_awake`'s cap cannot come down toward the intra-turn gap percentiles, because 6 of those 101 runs have combined main-plus-subagent gaps over 15 minutes and 2 over 30 — so the clock still false-releases at today's window. And `waiting_settle` should not take a silence clock at all: it argues from a positive completion signal that `subagent_gate` already reads out of `agent-<id>.jsonl`, and a wrong `Done` is the more expensive error, being what `attention` and the `done` notification rule key on.

Worth carrying forward from it: the file-to-row mapping it called missing is not missing. Every transcript path carries its owning session uuid as the second path component, and `ChatIdRegistry::anchored` is already the persisted lookup from there to a chat_id.

## Priority, and the measurement that would settle it

Low. This is two modules disagreeing rather than a turn-killing defect, and the case for `idle_awake` holding anything at all currently rests on 49 seconds.

Before building it, re-run both reconstructions — caffeinate intervals from `pmset -g log` keyed by assertion id and pid, and `idle_awake` hold intervals from `widget.jsonl` — after a few more weeks, and classify each uncovered gap by whether the row was mid-turn or at a turn boundary. If the answer stays near zero, the question is not how to tune the silence bound but whether `idle_awake` earns a hold at all. If mid-turn gaps appear, the bound matters more than this memo allows.

Three shipped sentences claimed a bad release "is corrected by the agent's next output, which re-takes the assertion within a tick" — the module doc on `idle_awake`, the `idle_awake_silence_ms` entry in `docs/pages/settings.md`, and the `idle_awake` paragraph in this repo's `CLAUDE.md`. All three were corrected on 2026-09-26 and are recorded here only so the measurement behind them is not lost: the two corrections took 3m16s and 27m46s, and both re-holds followed the workflow's task notification waking the agent rather than any output the watcher can see, so the blindness causing the release is the blindness delaying the correction. Whatever this memo is eventually settled as, that number is what any new wording has to stay true to.
