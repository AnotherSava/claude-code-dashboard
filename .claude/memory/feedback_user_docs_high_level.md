---
name: feedback_user_docs_high_level
description: Keep user-facing docs high-level; omit rare edge cases and minor details
metadata: 
  node_type: memory
  type: feedback
---

User-facing documentation (docs/pages/, README) should read as a high-level description. Don't enumerate every small detail, rare use case, or edge-case behavior — users skim for the gist. Example: for the rename box, "Enter saves, Esc cancels" is enough; the empty-value-is-treated-as-cancel nuance does not belong in the doc even though it's real behavior in the code.

**Why:** Exhaustive edge-case listings make docs noisy and harder to skim; the precise behavior lives in code/comments where it belongs.

**How to apply:** When documenting a feature, state the primary behavior plainly. Leave defensive fallbacks and uncommon branches out unless a user would realistically hit and be confused by them. Edge cases that matter for maintainers go in code comments or the Development docs, not the user pages. Related: [[feedback_about_what_not_how]].

**Two corollaries from a docs-review session:**
- **Intros stay at altitude.** A section/page lead-in (the text above the sections) is orientation, not a feature list or a specific interaction. Don't open the Features page by explaining how to hide the widget to the tray — let the sections below carry the specifics. The intro answers "what is this and why do I care", one or two sentences.
- **Avoid UI/dev jargon** an unprepared reader won't recognize. Prefer "status badge" over "status pill"; lead prose with a plain word rather than a bare code span. Jargon that's precise for maintainers belongs in the Development docs.
