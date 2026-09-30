---
name: status_variant_is_a_wire_break
description: Adding a Status variant rejects the whole sync push on an older peer and wipes prompt_history.json on a downgrade — why the clean state reused two variants instead
metadata:
  type: project
---

Adding a seventh `Status` variant was the obvious way to add a CLEAN state on 2026-09-30, and it was rejected on two hazards that the compiler does not show you. The redefinition that shipped instead reuses the six (`Done` absorbed read/unread and became the evidence-free sink; `Idle` became CLEAN), so nothing here has been paid — but the next status idea will reach for a variant again.

**The sync wire rejects the whole push, not the row.** `SessionSync` carries a whole `AgentSession`, `Status` has no `#[serde(other)]`, and `post_sync` takes the body through axum's `Json` extractor. So one row in a new state makes an older peer answer 422 to the *entire* push — every row from the upgraded device freezes and is TTL-reaped after 90s, not just the new-state one. The two machines are deployed independently, so that window is real, and the rejection is logged at `debug!`, which is below the default level: the peer's board emptying looks exactly like the peer being asleep. `Activity` (in `session_registry`) has the `#[serde(other)] Unknown` arm that would fix this; `Status` deliberately does not, because every `Status` value drives behaviour (sleep holds, notification keys, glyphs) so the fallback would have to be a lie chosen in advance.

**A downgrade wipes the dialog history.** `DialogEntry::status` is a required serde field and `PromptHistoryStore::new` swallows a parse error with `unwrap_or_default()`. Write one entry carrying the new variant, then run an older binary — a rollback, or the release build alongside a local deploy — and every session's stored dialog, `original_prompt` and task clock is silently replaced by an empty map and overwritten on the next save. `remote_history.rs` handles the same case better, with per-device files and a `warn!`.

Two smaller costs, both real and neither a compile error: the task-boundary rule in `apply_set` enumerates the resting statuses by hand in a `matches!`, so a new variant silently fails to start a new task; and the glyph map's forward half is compile-fenced while `status_from_glyph`'s wildcard is not, so a missing inverse arm ships and only shows up after a restart.

For the price of the *variant* itself, the `Waiting` precedent (commit d577efe) is the measurement: 24+ files, including four docs pages, the `investigate` skill and CLAUDE.md.

**What to do instead**, in order of preference: redefine existing variants where the meanings genuinely partition that way (what happened here); a field beside `Status` in the `instruction_drift` / `terminal_stale_at` shape, which is `#[serde(default)]` and therefore parses on an older peer; or, if a variant is truly unavoidable, retrofit `#[serde(other)]` onto `Status` and decide the lie it tells *first*.
