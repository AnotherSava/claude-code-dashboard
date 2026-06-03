---
name: feedback-favor-clean-design
description: Backwards compat is not a priority — prefer cleaner design over maintaining legacy fields
metadata: 
  node_type: memory
  type: feedback
---

Backwards compatibility is not a priority in this project. When a cleaner design requires breaking changes (removing fields, changing types, reshaping the data model), prefer the clean design over keeping legacy fields for compat.

**Why:** User explicitly said "backwards compatibility is not a priority, you can sacrifice it in favour of cleaner design" when I proposed keeping `previous_prompts` alongside the new `dialog` field for tooltip compat.

**How to apply:** When refactoring data models, don't add new fields alongside old ones "to avoid regression" — replace the old field and update all consumers in the same change.
