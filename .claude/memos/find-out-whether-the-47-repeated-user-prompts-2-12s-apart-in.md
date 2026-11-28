---
created: 2026-11-27 21:12:23
---

# Find out whether the 47 repeated user prompts 2-12s apart in prompt_history.json are echoes or real repeats

Measured 2026-11-27 while fixing the queued agent-message echo (state.rs is_queued_echo, QUEUED_ECHO_MS): beyond the 31 echo pairs under 268ms, prompt_history.json holds 47 identical consecutive user entries 2.3s to tens of seconds apart, mostly typed by a person ('y', 'why don't you add a switch between light...'). Some may be genuine repeats; others may be the same watcher-plus-hook echo for a person's prompt queued mid-turn, with a longer gap than the 1s window covers. Next step: for a few pairs, find the transcript's queued_command attachment and the UserPromptSubmit classify line in widget.jsonl, and see which writer made each copy.
