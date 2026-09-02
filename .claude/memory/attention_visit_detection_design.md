---
name: attention-visit-detection-design
description: Departure-marking behind a terminal adapter; an fsevent watch on agterm's window snapshot is the edge source, the poll the safety net
metadata:
  type: project
---

Polling alone cannot do this. Sampling a *level* (`tree --json`'s `active` session) to detect an *edge* (the user left that tab) misses any visit shorter than the tick, and the tick is pure *discovery lag* rather than a chosen delay — so no interval closes it. Confirmed twice in production 2026-09-02: a finished session opened and left inside one interval stayed `Done`.

**Two upstream asks were made of the agterm project and then withdrawn on 2026-09-02** — a selection-change event kind, and a one-shot `agtermctl events` that prints its cursor. Do not re-raise them. The event route only pays off if the dashboard *writes* `session status … --auto-reset`, since that is what makes a visit emit a `status` event; the glyph write is unwanted (see [[agterm-status-coexistence]]), so a one-shot read would arrive with nothing worth reading.

**Mark on departure, not on arrival** (decided 2026-09-02). Leaving a finished session's tab is what marks it read — that is the moment you are done with it. Three consequences:

- It was **one line, not a new model**. The read test stays `attended_at >= content_at()`; only *which* instant is stamped changes. The tempting "content settled inside `[T_enter, T_leave]`" is **wrong at the lower bound** — a row that finished at 09:00, was opened at 10:00 and left at 10:10 was read, and an interval test rejects it. Only `T_leave` matters. Keep `T_enter` solely if a minimum-dwell threshold is ever wanted (deliberately not wanted now: it buys "glanced for two seconds" and costs a tunable nobody can pick without watching real use first).
- It **fixes a defect in the shipped version**, which stamps on *arrival* — so merely passing through a finished tab marks it read without a word being seen. Arrival-marking was removed, not kept alongside.
- **Err early on `T_leave`.** Stamping it late marks content that arrived after the user left, which hides it; stamping early only leaves a row showing. Subtract the write debounce rather than using the observation time raw.

Deliberately uncovered, in the safe direction: read a finished session, then walk away without switching — unmarked until you return and switch away. Accepted; no backstop.

**Built 2026-09-02**, both halves: departure-marking behind `terminals::TerminalAdapter` (`terminals::agterm` the only implementation), and the edge source below.

- **Primary, edge-triggered (`AgtermAdapter::watch`): watch `<agterm-app-support>/windows/<windowUUID>.json`.** One file per window, top-level `selectedSessionID`. Written by `AppStore.scheduleSave()` on a 0.3s debounce, so a selection change reaches disk in ~300ms and a short visit writes *twice* — entering and leaving — which is the property polling structurally cannot have.

  **The schema, read from the live file 2026-09-02:** top level is `version` (1), `selectedSessionID`, `workspaces`, `sessionRecency`, `sidebarMode`, `sidebarVisible`, `sidebarWidth`; each `workspaces[].sessions[]` entry carries `{id, cwd, flagged, fontSize, isSplit, splitCwd}`. **There is no `title`** — and that is the constraint that shapes the design, because `attention::resolve_row` prefers the title over the cwd (a `cd`-ed session reports a cwd deriving *another* row's id). So a departure still costs one `agtermctl tree --json` to name the session, with the snapshot's own `cwd` as the fallback when that call fails. It is an edge-triggered subprocess — one per real tab switch, bounded by how fast a human switches — not a per-tick one.
- **Safety net: the poll, already gated.** It runs only while some row is finished-and-unread (nothing unread ⇒ no visit can mark anything ⇒ zero subprocesses, most of the day). **It does not need to be fast**, and an earlier "1s" figure here was wrong: every `Observation` carries an absolute instant, so a late reading reports the same instant as a prompt one and only the pill's latency suffers — 30s is what shipped, the watch having taken over the edge. Not redundant with the watch: it uniquely carries `idleMs`, enumerates windows, and covers a schema change, a permission problem or a dropped fsevent.
- **Not built: let the poll audit the watch.** If the poll saw `active` change for a window and the watch produced no edge for it, the file path has broken. Worth adding — it costs nothing, since the adapter already reads `active` for its own departure detection, and it would turn the silent failure this feature has already had twice into a noisy one. Today a broken watch degrades quietly to poll-speed instead.

**Four things that will bite (from the agterm source, verified statically by the agterm session):**

1. **Watch the directory, not the files.** `PersistenceStore.save` uses `Data.write(options: .atomic)`, so the inode is swapped every write and a file-level watch goes deaf after the first one — indistinguishable from "the user stopped switching tabs". Treat create/rename/modify alike as "re-read this window".
2. **A write is not a visit.** `scheduleSave` is also called for rename, move, reorder, sidebar width and recency. Diff `selectedSessionID` per window and act only on a change.
3. **The debounce coalesces.** An enter-and-leave inside 300ms collapses to one write carrying the final state, and that visit is lost. This narrows the blind window ~16x (5s → ~300ms); it does not close it. Write it down as a bound, not an exactness — and note the bound comes from the source constant, not from measurement.
4. **Gate on `version` and degrade loudly.** Private, undocumented state with no compatibility promise. An unexpected `version` drops to the poll, logged — never a silent misparse. Atomic writes mean no torn reads, so a parse failure is a real signal rather than a race.

**Built behind a terminal adapter** (`terminals/`, 2026-09-02). Windows runs a different terminal and will need its own, so the agterm specifics must sit behind a seam from the first commit rather than being retrofitted. What varies per terminal is only *how the observations are obtained*; what stays common is everything after them. So the adapter's output is the seam: a departure (and an input instant) carrying the terminal's own way of naming a session — its working directory and its title, both of which any terminal has. Resolution to a row (`resolve_row`), the `attention()` verdict, `observe`, the decision log and the emit all stay generic and see no terminal at all.

Note the naming trap: `src-tauri/src/adapters/` already exists and holds *agent* adapters (`claude.rs`, hook payload → status). A terminal adapter is a different axis and does not belong in that directory under a name that implies it does.

**Confirmed live 2026-09-02:** `notify` on that directory delivers — the watcher logged `watching the selection snapshot` at start-up and a real tab switch produced `watch departed` with no perceptible lag. **Still unmeasured:** whether the real write cadence matches the 300ms bound; that figure comes from agterm's source constant, not from measurement here.

Related: [[agterm-status-coexistence]], [[notification-delivery-channel]].
