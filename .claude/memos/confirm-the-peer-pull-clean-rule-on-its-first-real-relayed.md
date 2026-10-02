---
created: 2026-10-01 17:52:20
---

# Confirm the peer-pull CLEAN rule on its first real relayed pull, and read the refusal reason off the log

The peer-pull CLEAN rule (`http_server::pull_declared_clean`, built 2026-10-01) has never settled a row CLEAN from a real relayed pull. Every success case was driven by synthetic hook events POSTed to `/api/event` with a hand-built envelope prefix — real HTTP surface, real state machine, but not a real peer message.

What is actually unobserved: a message from the other machine arriving through `sync::post_message`, Claude Code delivering it as a `UserPromptSubmit` whose prompt is the full `build_content` envelope, `is_relayed_prompt` recognising it off the wire rather than off a test fixture, `/pull` step 10 posting, and the turn's `Stop` settling IDLE. The only real claim seen so far (the `claude` session, 2026-10-01 00:15:07) was correctly refused on the status gate because its turn ended on a question, so it exercised a refusal path and not the success path.

This repo has been bitten by exactly this shape before: `session_launcher`'s "verified end-to-end" turned out to mean one of the two routes that reach the same refusal, and the other one was broken. Same risk here — the synthetic prompt was constructed by calling `build_content` in a test, so a difference between what that function returns and what Claude Code actually hands the hook (leading whitespace, a wrapper, truncation of a long envelope) would be invisible to everything that has run.

HOW TO CONFIRM, no setup needed — just wait for the next real one and read the log:

    L="$HOME/Library/Application Support/com.anothersava.claude-code-dashboard/widget.jsonl"
    grep -E '"(pull_claim|pull_clean)"' "$L" | tail -20

A success reads `pull_claim outcome=recorded` followed by `pull_clean` with NO `outcome` field. Any refusal carries an `outcome` naming which fact failed (`still_outstanding` / `not_relayed` / `not_clean_before`), and `not_relayed` on a genuinely relayed pull is the specific failure this memo is about — it would mean `is_relayed_prompt` did not recognise the real envelope, and the fix is to compare the stored `original_prompt` for that row against `RELAY_PREAMBLE` byte for byte.

A relayed pull happens on its own whenever the other machine runs `/commit` and pushes while this side is clean, so this needs no contrived trigger; it needs someone to look once it has happened. Same shape as the memo for confirming the subagent permission-prompt gate on its first real prompt.
