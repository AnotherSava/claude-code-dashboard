---
name: transcript_string_content_invisible
description: A typed prompt is written to the transcript with string `content`, which log_watcher's wire type skips, so infer_state never sees one
metadata:
  type: project
---

Claude Code writes a `user` transcript entry's `message.content` in two shapes: a list of blocks (tool results, interrupt markers, prompts carrying images) and a **plain JSON string** (typed prompts, slash-command messages such as `<command-message>adopt</command-message>`). `log_watcher::TranscriptMessage::content` is `Option<Vec<TranscriptBlock>>`, so a string-content entry fails to deserialize and `infer_state` skips the whole line (`Err(_) => continue`). Measured 2026-09-27: 66 string-content and 933 list-content user entries in one session's transcript.

Consequence: `infer_state`'s "a fresh user prompt resolves to Working" branch only ever fires for list-shaped prompts, and the comment beside the interrupt-marker check ("a fresh prompt afterwards would be newer") holds only for those. In practice `UserPromptSubmit` sets `Working` first, so no wrong status has been traced to it; the one reachable misread is a chunk holding an interrupt marker followed by a typed prompt, which resolves to `ended` rather than `Working`. Not fixed — found by a review of the 2026-09-27 AskUserQuestion watcher fix, and pre-existing.

**How to apply:** when reasoning about what the watcher sees of user input, count typed prompts as invisible to it; if a fix needs them, widen the wire type to accept both shapes (an untagged enum of string or block list) rather than adding a second parser.
