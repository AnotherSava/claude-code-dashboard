---
created: 2026-11-24 15:27:44
---

# Raise a running session's tab in pick-local-session.py with an agterm:// link instead of session select + window select

Suggested by the agterm session, 2026-11-24.

agterm v0.44.0 registers the agterm:// scheme with macOS. open "agterm://session/<SESSION_ID>" selects the session and raises its window in one step. It needs no window id, starts agterm if it is not running, and brings agterm to the front.

Where: _raise_tab in integrations/pick-local-session.py, the loop over ("session","select","--target",session) and ("window","select",window), together with the comment above it about the two calls taking their ids differently. A grep of src-tauri/src/terminals/ and src-tauri/src/agterm.rs on 2026-11-24 found no Rust path that raises a tab this way. Background paths (--no-select launches, attach_hosted's background tab) must stay as they are, because the link always activates agterm.

Constraints: it works only once the running agterm is 0.44.0 or later. On 2026-11-24 /Applications/agterm.app held a 0.44.1-based build while the running instance was still 0.40.0 until relaunch. The Release bundle claims agterm: and Debug builds claim agterm-debug:, so a Debug instance never receives these links. A version gate like CONTEXT_SINCE/TINT_SINCE, or a fallback to the two selects below 0.44.0, needs deciding.

Related: the same release range ships the session.selected events and tree's lastSelectedAt that move-agterm-visit-detection-to-session-selected-and-idle.md is about.
