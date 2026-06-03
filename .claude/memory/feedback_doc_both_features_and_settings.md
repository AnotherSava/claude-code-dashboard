---
name: feedback_doc_both_features_and_settings
description: "A user-facing feature needs a features.md mention, not just a settings.md config entry"
metadata: 
  node_type: memory
  type: feedback
---

When a new capability is config-driven, document it in BOTH `docs/pages/features.md` (a high-level "you can do X" mention) and `docs/pages/settings.md` (the config-field reference). Updating only the settings reference is incomplete — the features page is what users skim to discover the capability exists.

**Why:** Settings docs answer "what does this knob do" for someone who already knows the feature; the features page answers "what can this app do" for someone who doesn't. A config-only doc update leaves the feature undiscoverable. (Triggered by "did you update github pages?" after I'd touched only settings.md for `context_alert_percent`.)

**How to apply:** After adding a config field for user-facing behavior, also update the relevant section of `docs/pages/features.md` with a one-line mention, and check whether the README + `docs/index.md` taglines still hold. Keep it high-level per [[feedback_user_docs_high_level]].
