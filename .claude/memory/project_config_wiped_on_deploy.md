---
name: config-wiped-on-deploy
description: deploy overwrites config.json wholesale; persist runtime-mutated state in its own app-data JSON file
metadata: 
  node_type: memory
  type: project
  modified: 2026-08-07T07:31:14.599Z
---

The `deploy` step `cp -f`'s `config/local.json` over the installed `config.json`, so `config.json` is replaced wholesale on every deploy and any field not in `local.json` reverts to its serde default.

**Why:** This means runtime-mutated state stored in `config.json` is wiped on each deploy. Window positions (`window_position`, `history_window_position`) already reset this way; `custom_names` would have too, which is why it was moved out of `config.json` into a dedicated `CustomNamesStore` (`custom_names.json`). The behavior isn't visible from the repo — it lives in the global deploy skill plus a gitignored `config/local.json`.

**How to apply:** When persisting durable, runtime-mutated, per-project/session state, give it its own JSON file in the app data dir (the established pattern: `prompt_history.json`, `session_chat_ids.json`, `custom_names.json`), NOT a `config.json` field. Reserve `config.json` for deploy-managed settings the user edits via `config/local.json`. Related: [[no-redundant-flags]] (use existing data), [[favor-clean-design]].

The reverse also follows from the same mechanism: to make a display/behavior setting default **on this machine's deployed install** (e.g. "make auto-resize up by default"), add the field to `config/local.json`, not the Rust `Default` impl in `config.rs`. The `Default` impl is the shared fallback used only when no `config.json` exists at all (a truly fresh install); on this machine `local.json` always wins on every deploy regardless of what `Default` says. (2026-08-07: did this for `auto_resize: "up"`.)
