---
created: 2026-09-08 03:09:00
---

# Fix the snapshot-to-publish race in commands::emit_sessions_updated that leaves a finished row showing WORK.

OBSERVED 2026-09-08T04:44:04Z on bga-assistant (console pid 32604): a turn ended, the row went Done, and both the widget row and the terminal tab showed blue for 4m41s until the user read the session. LOG EVIDENCE, all within 13ms, from widget.jsonl: .494 event -> set decision=classify status=Done; .500 apply_set prior_status=Working new_status=Done; .507 terminal title written title="green-circle bga-assistant"; .507 terminal title written title="blue-circle bga-assistant"; .856 event sessions_updated x2. Note the two title writes in one millisecond and the two emits - the stale blue one lands SECOND and wins. MECHANISM: emit_sessions_updated takes its snapshot with display_snapshot at the top of the function and only then calls terminal_title::sync, lid_awake::sync, tray_badge::refresh and app.emit, holding no lock across that pair. Two callers ran concurrently on that turn - the Stop hook path (http_server -> set -> emit) and the log_watcher, which fires moments later when Claude Code flushes the final assistant message, a lag the design documents and relies on. The watcher snapshotted BEFORE apply_set at .500 and so was carrying Working; the hook path snapshotted after and was carrying Done. They then serialise INSIDE terminal_title::sync on its own pids/last mutexes and inside push_title on the console attach lock, so the two writes are ordered - but each still carries the snapshot it took earlier, so the later WRITE won with the earlier READ. The two sessions_updated payloads at .856 have the same defect, which is why the widget row was blue and not just the tab glyph. WHY IT PERSISTED: terminal_title::sync runs only from emit_sessions_updated and nothing calls that on a timer, so with no further state change on the row nothing re-asserted the title; every attention poll from 04:44:15 to 04:48:45 read back the blue title. REASSERT_MS does not help because it only applies once sync is called. It corrected only when the read at 04:48:45 triggered the next emit, which wrote the white idle glyph. WHAT WAS NOT WRONG, and worth stating so nobody widens the hunt: AppState held Done throughout, provable because attention_seen fired at 04:48:45 and the read flip applies only to a row whose attention verdict is pending, which requires Done. Notifications, GET /api/agents and the sync push were all correct; the corruption was display-only. This is a general race at every turn end rather than anything specific to this agent - it only becomes visible when the watcher's emit loses the ordering coin flip, which is why it is rare and why a clean log is not evidence of absence. FIX: move the snapshot inside a mutex that also covers the sync calls and the emit, so snapshot-to-publish is atomic per call; two concurrent callers then each publish a self-consistent view and the later one wins with the newer state. It cannot deadlock - nothing inside the emit path re-enters emit_sessions_updated, and stale_check::request is a non-blocking channel send. REJECTED ALTERNATIVE: stamping each write with the row's updated field and having sync refuse a staler snapshot fixes the tab glyph but not the frontend emit, which carried the same stale payload and is half the symptom. ALSO CHECK emit_sessions_updated_remote, which has the same shape - snapshot taken outside any lock - though it only emits and writes no title.

[DONE 2026-09-18, and this memo's prescribed fix is the one that failed. The mutex it asks for
— taken at the top of `emit_sessions_updated`, covering the snapshot, the sync calls and the emit
— was written, gated, deployed and REVERTED the same day. "It cannot deadlock" is wrong: the
hazard is not re-entrancy on one thread, which the memo checked correctly, but a cross-thread
mutual wait. The emit body reaches `lid_awake::sync` -> `tray::sync_lid_awake_state` -> four
`CheckMenuItem::set_checked`, and `tray_badge::refresh` -> `set_tooltip` / `scale_factor` /
`set_icon`; each is `run_item_main_thread!` or `window_getter!`, which posts to the Tauri event
loop and blocks on `rx.recv()` — and a sync `#[tauri::command]` runs INLINE on that same main
thread, so `set_chat_name`, `open_history` -> `attention::observe`, `close_window`/`hide_history`
-> `mark_history_target_read` and the tray menu are all paths from the main thread into the same
lock. A background emit holding it while waiting for the main thread, against a click that takes
it, kills the event loop: widget, tray and history window frozen, and tray Quit needs the main
thread too, so the process must be killed. Three independent review lenses confirmed it.

What shipped is this memo's REJECTED ALTERNATIVE, and the rejection was half right.
`AppState::snapshot_versioned` mints a monotonic ticket INSIDE the `sessions` mutex — so lock
order is content order, which a ticket minted after the clone would not give — and
`terminal_title::sync` compares it against the newest it has written from, standing down when its
own is older, logged `decision = "title_seq_stale"`. Nothing is serialized, so no caller waits on
another and no main-thread hop can sit under a contended lock.

The frontend emit is knowingly left unordered, which is the half the rejection was right about. It
carries no surface that outlives the emit, so the next one repaints it; the tab title does outlive
the process, which is the whole reason that half was worth fixing and this half was not, yet. See
[[node_toolchain_pin_ahead_of_node]]'s sibling note in `learnings/tauri-main-thread-deadlock.md`
for the mechanics before any second attempt.]
