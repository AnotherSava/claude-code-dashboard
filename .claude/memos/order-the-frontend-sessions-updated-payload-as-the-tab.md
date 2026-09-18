---
created: 2026-09-18 16:41:12
---

# Order the frontend sessions_updated payload, as the tab titles now are

Two `commands::emit_sessions_updated` calls can be in flight at once. Each snapshots `AppState` and then races the others for the locks downstream, so the one holding the OLDER snapshot can publish last. On 2026-09-18 the tab-title half of that was fixed: `AppState::snapshot_versioned` mints a monotonic ticket inside the `sessions` mutex, and `terminal_title::sync` stands down when its ticket is older than the last it wrote from, logged `decision = "title_seq_stale"` (7 stand-downs observed in the first hours). The frontend half was left: `app.emit("sessions_updated", sessions)` in the same function still publishes an unordered snapshot, with `seq` in scope and unused a few lines above it.

WHY IT WAS LEFT, so the trade is visible rather than forgotten. A stale payload is corrected by the next emit, and in the common case an emit is already queued behind it. A tab title has nothing to correct it — it outlives the process, so a wrong glyph sits there until something else happens to move it, measured at three minutes on 2026-09-18, which is the bug that started the work. The exposure is real all the same: in a quiescent stretch with no further emit, the widget can show a status the row does not hold, and nobody has measured how often that happens because no log line records it.

HOW TO FIX IT. Carry the ticket in the payload and have `App.svelte` drop a payload whose seq is lower than the highest it has seen. Lock-free and correct by construction. That changes the `sessions_updated` payload shape from `Vec<AgentSession>` to something carrying the seq beside the rows, which is a frontend-visible change and the reason it was not folded into the title fix.

HOW NOT TO FIX IT. Do NOT reach for a mutex held across `app.emit`. `emit` serde-serializes the whole payload inline before posting (measured ~6 MB across nine rows), so the lock would be held for tens of milliseconds, and the Tauri main thread enters `emit_sessions_updated` through the sync commands `set_chat_name`, `open_history` -> `attention::observe`, `close_window`/`hide_history` -> `mark_history_target_read`, and through the tray menu. A global `EmitGate` doing exactly that was written, gated, deployed, and reverted the same day after three independent review lenses confirmed it deadlocks the event loop — widget, tray and history window frozen, and tray Quit needs the main thread too, so the process must be killed. The mechanics are in `learnings/tauri-main-thread-deadlock.md`; read it before any second attempt.

Also worth checking at the same time: `emit_sessions_updated_remote` has the same shape — snapshot taken outside any lock — though it only emits and writes no title.
