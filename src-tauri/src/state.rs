use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// CLEAN: a confirmed claim that there is nothing here to come back to.
    ///
    /// **Settable only on positive evidence**, and that prohibition is the whole
    /// point of the variant. The three sources are a `SessionStart` whose
    /// `source` is `clear` or `startup` (`adapters::claude`), a `resume` whose
    /// restored dialog ends in a [`BoundaryKind::Clear`] separator
    /// (`http_server::resume_is_clean`), and a relayed peer pull that reported
    /// leaving nothing behind, on a row that was already clean when the request
    /// arrived (`http_server::pull_declared_clean`). Nothing else may write it —
    /// above all not a
    /// degrade path, which is what five of them used to do back when this variant
    /// was also the "we know nothing" sink.
    ///
    /// An absence of evidence lands on [`Status::Done`] instead. Reaching for
    /// `Idle` there claims the user has nothing to return to, which hides real
    /// work — the one forbidden direction in this state machine.
    Idle,
    Working,
    /// Held active by background work after the main turn already settled —
    /// "looks done but isn't". Set at `Stop` time from the hook's
    /// `background_tasks` payload (see `adapters::claude::classify_stop`); the
    /// next turn's `Stop` (empty `background_tasks`) settles it to `Done`.
    /// Rendered light-blue as "WAIT".
    Waiting,
    /// Blocked on the user: a question, a tool-permission prompt, or an MCP
    /// elicitation. Rendered amber as "BLOCK". (Formerly `Blocked`.)
    Blocked,
    /// "A turn ended here." Both the settled end of a turn and the neutral
    /// landing place for every absence of evidence.
    ///
    /// It carries no claim about whether the user has *seen* the result — that
    /// is [`Attention`], flattened onto the row as [`AgentSession::read`] by the
    /// machine that ran the session: `commands::display_snapshot` for a local
    /// row, and `sync::ingest` for a synced one, from the verdict its origin
    /// advertised. Status used to carry it
    /// (`Done` meant finished-and-unread, and a display-time rewrite turned a
    /// read row into `Idle`), which is why `Idle` ended up meaning three
    /// unrelated things at once.
    ///
    /// Being the evidence-free sink is the other half, and it is why every
    /// degrade path points here: a stale `Working` restored from a tab, an
    /// Esc-cancelled first turn, a `Waiting` the backstop timed out, a row
    /// invented because a subagent asked for permission. None of those know the
    /// user has nothing to come back to, and [`Status::Idle`] would assert it.
    #[default]
    Done,
    Error,
}

impl Status {
    /// True when a turn is in flight and sleeping the machine would suspend it.
    ///
    /// `Waiting` counts: the main turn settled but a background shell task or
    /// subagent is still running. `Blocked` does not — the agent is parked on
    /// the user, so nothing progresses while the Mac is asleep anyway.
    ///
    /// Both sleep holds read this — [`crate::lid_awake`] for the lid-closed
    /// veto and [`crate::idle_awake`] for the idle-sleep assertion — and they
    /// have to agree on what counts as work, since a row one of them protects
    /// and the other does not would sleep by whichever rule is laxer.
    pub fn is_live_work(self) -> bool {
        matches!(self, Status::Working | Status::Waiting)
    }
}

/// Whether a finished row is still waiting to be looked at — the "I haven't read
/// this one yet" axis, orthogonal to [`Status`].
///
/// **The verdict is computed, never stored.** [`AgentSession::attention`]
/// derives it on demand, for a local row and a synced one alike:
/// `attention::should_poll`, which decides whether asking the terminal anything
/// is worth the cost; `commands::display_snapshot`, which flattens it into
/// [`AgentSession::read`] on the way to the frontend and the tab titles;
/// `sync::build_push`, which advertises this machine's own rows to the user's
/// other machines; and `AppState::mark_remote_attended`, which asks it either
/// side of stamping a synced row. The flattened flag, not this enum, is what
/// crosses a wire.
///
/// That flattening is the only way the distinction reaches a reader, because
/// `status` no longer carries it. It used to: `Done` meant finished-*and-unread*
/// and a display-time rewrite turned a read row into `Idle`. That overloading is
/// what left `Idle` meaning three unrelated things, and undoing it is what freed
/// `Idle` to mean CLEAN.
///
/// This exists because nothing else in the process can answer it. `idle.rs`
/// reports input across the whole desktop, not per session, so a `Done` row the
/// user read ten seconds ago and one he has never opened are byte-identical
/// today — same `status`, same `state_entered_at`, same dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    /// This row isn't asking to be read, so "looked at" carries no meaning.
    /// Every non-`Done` row, synced ones included — a tab on this machine can
    /// render another machine's agent, so this device does make observations
    /// about synced rows, beside the verdict their origin pushes.
    Moot,
    /// Finished, and not looked at since it finished.
    Pending,
    /// Finished, and looked at since it finished.
    Seen,
}

/// Instruction-adherence canary status for a session — colors the agent name in
/// the dashboard (stamped for local rows by `commands::resolved_snapshot`; read by
/// `SessionItem.svelte`). Ordered by certainty: `Alive` is only claimed once the
/// marker has actually been observed, so a set-up-but-unconfirmed session reads
/// `Pending` rather than falsely vouching green.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Canary {
    /// Not set up: the feature is off, or no nonce exists (session predates the
    /// feature, or an app restart lost the in-memory nonce).
    #[default]
    Off,
    /// Set up but not yet confirmed — the marker has not been observed on any turn
    /// yet, so we can't vouch it's working (the instruction may never have reached
    /// the model). Deliberately distinct from `Alive`: no false certainty before
    /// the agent has echoed the marker at least once.
    Pending,
    /// Set up and confirmed adhering — the marker has been observed and the latest
    /// checkable turn still carried it.
    Alive,
    /// Set up but drifted — the agent dropped its marker after prior adherence.
    Dead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogRole {
    User,
    Assistant,
    Separator,
}

/// What produced a [`DialogRole::Separator`] — the fact that makes a trailing
/// separator usable as evidence rather than as a four-way ambiguity.
///
/// `append_boundary` is shared by `take_session` (which runs for **every**
/// `SessionEnd` reason and for the liveness reaper) and by
/// `mark_session_boundary` (`PreCompact`), so until this existed the marker for
/// "the user wiped the context" was byte-identical to the marker for "the agent
/// exited with its conversation intact on disk" and for "the conversation was
/// compacted and continues". Only [`BoundaryKind::Clear`] licenses
/// [`Status::Idle`] on the following `resume`; everything else is a conversation
/// somebody may want back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoundaryKind {
    /// `/clear` — `SessionEnd` with `reason == "clear"`. The context is gone.
    Clear,
    /// `PreCompact`. The conversation continues under a summary, so its value
    /// survives even though the boundary looks the same.
    Compact,
    /// The session ended some other way — a plain exit, Ctrl-D, a closed
    /// terminal, or the liveness reaper stepping in. The transcript is still on
    /// disk and `--continue` will bring it back.
    Ended,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DialogEntry {
    pub role: DialogRole,
    pub text: String,
    pub timestamp: i64,
    pub status: Status,
    /// True when this entry is a user prompt that started a fresh task — the
    /// same boundary decision `apply_set` uses for the sticky label. The
    /// frontend reads this directly for the history highlight and the row
    /// tooltip, instead of re-deriving boundaries with a divergent heuristic.
    /// `#[serde(default)]` so dialogs persisted before this field existed load
    /// as `false` (those pre-flag entries simply aren't highlighted).
    #[serde(default)]
    pub task_start: bool,
    /// For a [`DialogRole::Separator`], what produced it; `None` on every other
    /// role and on separators persisted before this field existed.
    ///
    /// `#[serde(default)]` so an old `prompt_history.json` loads, and the `None`
    /// it yields is the safe reading: an untagged separator does not license
    /// [`Status::Idle`], so a dialog written by an older build is treated as a
    /// conversation somebody may want back rather than as a confirmed clear.
    #[serde(default)]
    pub boundary: Option<BoundaryKind>,
}

/// Built by the adapter, converted to a full [`DialogEntry`] by `apply_set`
/// (which adds `timestamp` and `status`).
#[derive(Clone, Debug)]
pub struct PendingDialogEntry {
    pub role: DialogRole,
    pub text: String,
}

/// Fields persisted to `prompt_history.json` and restored on session
/// re-creation after an app restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedSession {
    #[serde(default)]
    pub dialog: Vec<DialogEntry>,
    #[serde(default)]
    pub original_prompt: Option<String>,
    /// `AgentSession::delegated_task`, kept with the prompt it stands for.
    #[serde(default)]
    pub delegated_task: Option<String>,
    /// `AgentSession::message_line`, likewise.
    #[serde(default)]
    pub message_line: Option<String>,
    #[serde(default)]
    pub task_started_at: i64,
}

/// The text of the widget row's task line, and which kind it is.
///
/// Two kinds because the row draws them differently: current text plainly, a
/// past task muted and italic, so it does not read as what the agent is doing
/// now. On the wire as `{"kind": "current" | "past", "text": …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum RowLine {
    /// [`AgentSession::primary_text`]: what the row is about now.
    Current(String),
    /// The most recent task in the dialog, for a row with no current text.
    Past(String),
}

/// One task in the widget row's hover tooltip: when it began and the text it
/// shows as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLine {
    pub at: i64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub status: Status,
    /// The status the row held immediately before its current `Working` turn
    /// began — captured by `apply_set` on every non-Working → Working
    /// transition. A turn cancelled with Esc (no `Stop` hook) reverts here
    /// rather than collapsing to `Idle`, so an aborted reply to a pending
    /// question leaves the row in the `Blocked` state the question put it in
    /// (the next real answer is then an approval-cycle reply, not a new task).
    /// Internal bookkeeping — never serialized to the frontend / sync / disk.
    #[serde(skip)]
    pub status_before_working: Status,
    pub label: String,
    pub original_prompt: Option<String>,
    /// When another agent began the row's task: that agent's own task, the one a
    /// person gave it at the start of the chain, as resolved when the prompt
    /// arrived. `None` for a task a person began, and for one whose sender's
    /// task could not be resolved (see [`message_line`](Self::message_line)).
    /// See `prompt_origin`.
    ///
    /// For a relayed message it is the sending dashboard's record of that task,
    /// the envelope header's copy — reserved words `[redacted]`, quotes dropped,
    /// cut to 200 characters.
    ///
    /// A field beside `original_prompt` rather than a rewrite of it, because the
    /// two are different facts: `original_prompt` is what arrived (the whole
    /// envelope, kept for the history), this is what to call it. It moves in
    /// lockstep with `original_prompt` (`label_policy::select`), so it can never
    /// describe a task the row has left.
    ///
    /// Settled once, when the prompt arrives, and never recomputed: the sender
    /// moving on to other work does not change what this row was asked. Persisted
    /// with the prompt and carried on the sync wire, `#[serde(default)]` on both,
    /// so an older `prompt_history.json` loads and an older peer's push parses.
    #[serde(default)]
    pub delegated_task: Option<String>,
    /// When another agent began the row's task and its own task could not be
    /// resolved: the first line of the message, envelope stripped, which is the
    /// next-truest account of what this session was asked. `None` otherwise;
    /// never set together with [`delegated_task`](Self::delegated_task).
    ///
    /// Its own field because it is a different fact from a resolved task: a line
    /// another agent wrote, not something a person asked for. Kept in
    /// `delegated_task` it would be read back as one — by the next message this
    /// row sends (`prompt_origin`'s chain, `http_server::sender_task`), which
    /// would then report a message line, possibly the receiver's own message
    /// echoed back, as this row's task. Moves, persists and syncs exactly as
    /// `delegated_task` does.
    #[serde(default)]
    pub message_line: Option<String>,
    #[serde(default)]
    pub task_started_at: i64,
    #[serde(default)]
    pub dialog: Vec<DialogEntry>,
    pub source: String,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub updated: i64,
    pub state_entered_at: i64,
    pub working_accumulated_ms: u64,
    /// Only meaningful while `status == Waiting`: `true` when the WAIT is held
    /// by at least one silently-killable background task (a `shell` command),
    /// which the `waiting_settle` time-backstop must cover. A subagent-only WAIT
    /// leaves this `false` — a background subagent always resolves with a
    /// completion turn, so time-settling it would falsely mark a live subagent
    /// Done. Set by `apply_set` from the `Stop` classification. Internal
    /// bookkeeping, never serialized — so it cannot survive a restart, and a WAIT
    /// restored without it would be one the backstop can never settle. That is
    /// one of the reasons `session_restore::restored_status` refuses to bring a
    /// row back in `Waiting` at all unless the registry independently reports a
    /// turn running, and where it does the work is genuinely in flight and its
    /// completion turn leaves the state.
    #[serde(skip)]
    pub waiting_backstop_armed: bool,
    /// User-assigned display name, resolved from `CustomNamesStore` at emit
    /// time (keyed by `id`). Always `None` in `AppState`; filled on the way
    /// to the frontend. Not persisted in `prompt_history`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Device name of the peer dashboard this session was synced from; `None`
    /// for sessions running on this machine. Stamped by `sync::ingest` (which
    /// also namespaces `id` to "{device}/{raw_id}"); the frontend renders the
    /// device badge from it. Always `None` in `AppState.sessions`.
    #[serde(default)]
    pub origin: Option<String>,
    /// Instruction-adherence canary flag (see `Config::instruction_canary_enabled`).
    /// `true` when the most recent final-message-bearing `Stop` was missing this
    /// session's rotating marker — an orthogonal "the agent stopped honoring its
    /// standing instructions" warning that rides *alongside* `status` (Done /
    /// Blocked / Waiting are untouched), rendered as a row badge, a `⚠` in the
    /// terminal tab title, and a Telegram ping. Set / cleared by
    /// [`AppState::set_drift`] from the `http_server` Stop check. Serialized to the
    /// frontend (unlike the `#[serde(skip)]` internals above) so the row can render
    /// it; a synced remote row carries it for its badge, while notifications /
    /// titles stay local-only by construction.
    #[serde(default)]
    pub instruction_drift: bool,
    /// When the session's terminal tab was found to have stopped showing this
    /// row's status, or `None` while it is keeping up.
    ///
    /// Two things set it, and they see different faults.
    /// `terminals::stale_check` compares what a surface displays against the real
    /// console title of the session in it, which catches a tab renamed to
    /// anything at all. `terminal_title::observe_caption` compares a caption
    /// against what this dashboard wrote, so it can only ever catch a tab frozen
    /// on one of our own strings — the weaker oracle, and the one that was here
    /// first.
    ///
    /// The instant *is* the flag, rather than a bool beside one: the badge only
    /// needs to know it is set, but the Telegram alert waits out
    /// `stale_tab_alert_ms` from here so you can notice the `≠` and fix the tab
    /// before your phone buzzes. A separate boolean would carry nothing the
    /// `Option` doesn't. Repeated confirmations leave the stamp alone, so the
    /// clock measures the fault rather than the last time it was noticed.
    ///
    /// Orthogonal to `status` in the same way `instruction_drift` is: the row is
    /// right and something outside it is wrong, so it colours nothing and gates
    /// nothing. Windows-only today, because both oracles are: one is that
    /// adapter's caption hook, and the other needs a terminal that implements
    /// `TerminalAdapter::front_readings`.
    ///
    /// It lives here rather than being stamped at emit time like `canary` and
    /// `name_shared_by` for one concrete reason: `notifications::reconcile` reads
    /// the raw `AppState` snapshot, so a fact written only into
    /// `commands::resolved_snapshot` is invisible to the notifier and could never
    /// raise an alert.
    ///
    /// Two causes produce it and nothing outside the terminal can tell them apart
    /// — a Windows Terminal tab given a custom name (which outranks the console
    /// title for good), and a leftover tab whose session has exited. Both are
    /// truthfully "a tab is showing a stale status for this session", which is
    /// what the badge and the alert say.
    #[serde(default)]
    pub terminal_stale_at: Option<i64>,
    /// Instruction-adherence canary status ([`Canary`]) — colors the agent name in
    /// the dashboard. Stamped at emit time by `commands::resolved_snapshot` for
    /// local rows from the live nonce store (`Off` in `AppState`, like
    /// `display_name`); remote rows stay `Off` — this device isn't running the
    /// canary for them.
    #[serde(default)]
    pub canary: Canary,
    /// Wall-clock ms of the last moment the user was *observed* attending to this
    /// session — opening its history window, or (macOS) producing input in the
    /// agterm window while this session was the selected one. The only field on
    /// this row written by a user action rather than by the agent.
    ///
    /// Compared against [`AgentSession::content_at`] by [`AgentSession::attention`],
    /// which `commands::display_snapshot` flattens onto the row as
    /// [`AgentSession::read`]. That flag is the only way the verdict reaches a
    /// reader, and it exists because `status` stopped carrying it: `Done` now
    /// covers read and unread alike.
    ///
    /// Internal bookkeeping — never serialized to the frontend, to sync, or to
    /// disk (mirrors `status_before_working`). Keeping it off the wire is
    /// deliberate: it is *this* machine's observation of *this* keyboard, while a
    /// remote row's timestamps are the sender's clock.
    #[serde(skip)]
    pub attended_at: Option<i64>,
    /// Wall-clock ms, **this machine's clock**, of the moment this device last
    /// saw a synced row's [`content_at`](Self::content_at) rise. `None` on every
    /// local row, where it would mean nothing: a local row's content is produced
    /// here, so its own timestamps are already on this clock.
    ///
    /// It exists because dropping the observation's instant turned the verdict
    /// into a tautology, and that is worth stating plainly rather than leaving
    /// for the next reader to re-derive. [`AgentSession::attention`] asks
    /// `attended_at >= content_at()`, which *is* the recency test: a stale
    /// observation leaves a local row `Pending`. A synced row's `content_at` is
    /// the origin's clock, so an instant from this machine cannot be compared
    /// against it — but stamping `content_at` itself instead satisfies that test
    /// by construction, and then any observation credits the row however old it
    /// is. Those are not rare: `terminals::agterm` mints an input instant as
    /// `now - idle` from the **desktop-wide** idle clock, so typing once into a
    /// tab and walking away with the terminal in front re-offers that same
    /// frozen instant every tick. Measured in this machine's own log, 132
    /// credited input observations ran to a median age of 186s and a maximum of
    /// 594s — all harmless only because the comparison discarded them.
    ///
    /// So the comparison moves rather than disappearing: `mark_remote_attended`
    /// credits only an observation at or after this instant, which puts both
    /// sides on one clock while the value it stamps stays the origin's. Arrival
    /// is necessarily later than production, so the residual error falls toward
    /// `Pending` — the direction that leaves work showing.
    ///
    /// Written by `sync::ingest` and `sync::store_pulled_dialog`, the two paths
    /// that can raise a synced row's `content_at`, and carried across the
    /// metadata replace like [`attended_at`](Self::attended_at). Internal
    /// bookkeeping, never serialized anywhere.
    #[serde(skip)]
    pub content_seen_at: Option<i64>,
    /// The name the **origin's** own tab carries for a synced row, where that
    /// machine renamed it. `None` on a local row and on a synced row its origin
    /// has not renamed, whose tab then shows the id this device already derives.
    ///
    /// Arrives on `sync::SessionSync::origin_label` and is read in one place,
    /// `attention::remote_labels`, as a third name a badged tab may be joined by.
    /// It is not a name to *display* — the receiver's own rename wins there,
    /// which is why `ingest` keeps clearing `display_name`. Custom names are
    /// per-machine by design, so without this the join works only where the same
    /// rename was typed on both machines.
    ///
    /// `#[serde(skip)]` like the two fields above: it reaches a row through the
    /// sync envelope rather than through the session object, and nothing
    /// persists it — the next push re-delivers it.
    #[serde(skip)]
    pub origin_label: Option<String>,
    /// Whether the turn currently in flight was begun by a relayed peer message
    /// rather than by something a human typed — captured by `apply_set` on the
    /// same non-`Working` → `Working` transition that captures
    /// `status_before_working`, from `SetInput::turn_from_relay`.
    ///
    /// Both facts are about *this turn*, and both are written by one line of one
    /// function at one instant, so they cannot come to disagree about which turn
    /// they describe. That is the whole reason this is captured at the transition
    /// rather than stamped when the frame is written into the session's inbox:
    /// across 115 measured `peer_write`s, 16 produced no arriving prompt at all
    /// and 4 arrived in one burst 3.8 to 8.1 hours later, so a stamp left waiting
    /// for "the next turn" to collect it would be collected by a turn that had
    /// nothing to do with it — a human-typed one, hours later.
    ///
    /// Internal bookkeeping — never serialized to the frontend, to sync, or to
    /// disk (mirrors `status_before_working`), so a restart loses it and the row
    /// falls back to not-relayed, which is the direction that declines to make a
    /// CLEAN claim rather than inventing one.
    #[serde(skip)]
    pub turn_from_relay: bool,
    /// When a `/pull` run told this dashboard it left nothing worth coming back
    /// to (`POST /api/session-clean`), as wall-clock ms — `None` whenever no such
    /// claim is outstanding.
    ///
    /// A *claim*, not a verdict: it says only what the skill observed about its
    /// own run, and `http_server::pull_declared_clean` decides whether it may
    /// become [`Status::Idle`]. It is recorded mid-turn, because `/pull` posts
    /// from inside the turn it is reporting on, and consumed at that turn's
    /// `Stop`; `apply_set` drops it on every entry into `Working`, so a claim no
    /// `Stop` ever came for cannot be redeemed by a later turn. That is the
    /// revocation rule the design asked for, and it falls out of the ordering
    /// rather than needing a timer.
    #[serde(skip)]
    pub clean_claim_at: Option<i64>,
    /// Whether this finished row has been looked at — the [`Attention::Seen`]
    /// verdict, flattened for the frontend and the tab title.
    ///
    /// **A local row's verdict is stamped at display time; a remote row's
    /// arrives already decided.** For a local row this is always `false` in
    /// `AppState` and set by `commands::display_snapshot` and nothing else,
    /// which is the same split the old status rewrite used to occupy and for the
    /// same reason: `resolved_snapshot` is what `/api/agents` serves, and a
    /// machine asking what this one's agents are *doing* must not be told
    /// whether a human here has looked at their screen. For a remote row it is
    /// written by `sync::ingest` from `SessionSync::attended`, the origin's own
    /// verdict, so one row reads the same on both dashboards whichever machine
    /// it was read on — which is why `commands::stamp_read` still skips remote
    /// rows rather than judging them.
    ///
    /// The *observation* behind it ([`attended_at`](Self::attended_at)) stays
    /// machine-local in both directions. Only the verdict crosses, and only on
    /// the sync push: the origin computes it from its own full dialog, so a
    /// receiver — whose dialog copy lags by construction while a pull is
    /// outstanding — never re-derives it and so can never answer `Seen` for an
    /// answer the origin still calls unread.
    ///
    /// It exists as a field because `status` no longer carries it. `Done` used
    /// to mean finished-*and-unread*, so a second field would have been a
    /// duplicate state signal; now that `Done` covers both and `Idle` means
    /// clean, this is the only place the distinction lives. The frontend renders
    /// it as the inverse of the CLEAN pill — grey text on a dark fill becomes dark
    /// text on a grey fill — `terminal_title::status_glyph` as ⚪ instead of 🟢,
    /// and `session_restore` reads that glyph back, which is why nothing persists
    /// `attended_at` and nothing needs to.
    ///
    /// Threaded from the stamp rather than recomputed downstream: the stamp is
    /// gated on `config.attention_tracking`, and a second caller computing
    /// `attention()` for itself would ignore that gate and disagree with the
    /// widget.
    #[serde(default)]
    pub read: bool,
    /// How many things on this machine answer to this row's *name*. `1` is the
    /// ordinary case; anything above it means several terminal tabs carry the same
    /// caption, which is what the row's warning marker reports.
    ///
    /// Each local row sharing this row's display label counts at least once, and a
    /// row that several live sessions derive counts once per session. That covers
    /// both ways of arriving here with one number: a directory basename collision
    /// or a `--fork-session --resume` migration puts two sessions under one row,
    /// while two rows renamed alike put one under each. Reporting them separately
    /// would be two fields for one question — and the floor of one per row is what
    /// keeps this in step with `attention::resolve_row`, which refuses two
    /// identically labelled rows whether or not the registry sees a session behind
    /// either.
    ///
    /// `None` means not established, and is the honest answer in three cases: a
    /// remote row (this device does not run that machine's sessions), an
    /// unreadable session registry, and a build where `SessionRegistry` is not
    /// managed. It is deliberately not folded into `Some(1)`, which would assert
    /// a uniqueness nothing checked.
    ///
    /// Stamped at emit time by `commands::resolved_snapshot`, like
    /// [`AgentSession::canary`], so `AppState` never holds it and it is always
    /// `None` in the sync push (which reads raw `AppState`) — a peer's count would
    /// be about that machine's tabs anyway.
    #[serde(default)]
    pub name_shared_by: Option<usize>,
    /// The text of the widget row's task line, the [`AgentSession::row_line`]
    /// verdict. The frontend draws it rather than deciding it, so the row and
    /// the terminal headline `terminal_title` writes from the same function
    /// cannot disagree.
    ///
    /// Always `None` in `AppState`. Stamped by `commands::display_snapshot` for
    /// every row, local and remote, where `None` then means the row has no text
    /// for that line. Like [`AgentSession::read`] it is a display fact, so the
    /// sync push and `/api/agents`, which read the unstamped rows, never carry it.
    #[serde(default)]
    pub row_line: Option<RowLine>,
    /// The row's tasks as its hover tooltip lists them, oldest first, the
    /// [`AgentSession::task_lines`] verdict. Decided here rather than read off
    /// `dialog` by the frontend, because a task another agent began is its
    /// whole envelope there, kept for the history window, and the tooltip sits
    /// over the task line that shows the sender's task instead.
    ///
    /// Empty in `AppState` and stamped by `commands::display_snapshot` beside
    /// [`row_line`](Self::row_line), for the same reasons.
    #[serde(default)]
    pub task_lines: Vec<TaskLine>,
    /// The subagent permission prompts open on this row, and the main agent's
    /// own state underneath them. `None` whenever no subagent is waiting on a
    /// dialog, which is nearly always.
    ///
    /// While it is set the row reads `Blocked` with the newest prompt's label,
    /// but that is an overlay: every main-agent writer runs through
    /// [`with_base`], so a `Stop`, a new prompt or the watcher's promotion moves
    /// the base and leaves the BLOCK standing. When the last prompt settles, the
    /// base is written back exactly as stored, clock included.
    ///
    /// Internal and in-memory, like `status_before_working`: a restart loses it,
    /// and the row then keeps the BLOCK until the main agent's next event.
    #[serde(skip)]
    pub subagent_gate: Option<SubagentGate>,
}

/// A subagent's tool-permission dialog, read off its `PermissionRequest`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentPromptRequest {
    /// The hook's `agent_id`, never empty — what `SubagentStop` settles by.
    pub agent_id: String,
    /// The Claude Code session that raised it — what a main `Stop` with no
    /// background work settles by. Not the row: two instances can share a row
    /// (a `--fork-session --resume` migration leaves both in one cwd), and one
    /// instance finishing says nothing about the other's subagents.
    pub session_id: String,
    pub agent_type: Option<String>,
    /// The gated tool, `"tool"` when the payload names none.
    pub tool_name: String,
    /// The gated call's input as the hook reported it, `Null` when absent. The
    /// tick narrows same-name calls in the agent's transcript by it.
    pub tool_input: serde_json::Value,
    /// What the row shows while this is the newest open prompt.
    pub label: String,
    /// `<main transcript minus ".jsonl">/subagents`, where the agent's own
    /// transcript lives. `None` when the hook carried no `transcript_path`, in
    /// which case only the hook exits can settle the prompt.
    pub subagents_dir: Option<PathBuf>,
}

/// One open subagent prompt.
#[derive(Clone, Debug)]
pub struct PendingPrompt {
    /// Minted by [`AppState::open_subagent_prompt`]: starts at 1 and is unique
    /// for the life of the process, so two prompts from one agent never merge.
    pub request: u64,
    /// When the hook was applied — the tick judges the transcript against it.
    pub requested_at: i64,
    pub prompt: SubagentPromptRequest,
}

/// The four fields a pending subagent prompt overlays, as the main agent's own
/// events last left them.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseState {
    pub status: Status,
    pub label: String,
    pub waiting_backstop_armed: bool,
    pub state_entered_at: i64,
}

impl BaseState {
    fn capture(s: &AgentSession) -> Self {
        Self { status: s.status, label: s.label.clone(), waiting_backstop_armed: s.waiting_backstop_armed, state_entered_at: s.state_entered_at }
    }

    fn write_into(&self, s: &mut AgentSession) {
        s.status = self.status;
        s.label = self.label.clone();
        s.waiting_backstop_armed = self.waiting_backstop_armed;
        s.state_entered_at = self.state_entered_at;
    }
}

/// See [`AgentSession::subagent_gate`]. Exists only while `pending` is
/// non-empty; while it does the row's visible fields are `Blocked`, the newest
/// pending prompt's label, a disarmed backstop, and `blocked_since`.
#[derive(Clone, Debug)]
pub struct SubagentGate {
    pub base: BaseState,
    /// The visible BLOCK's `state_entered_at`. It keeps the row's own clock when
    /// the row was already blocked on the main agent, so one continuous BLOCK
    /// never restarts its count.
    pub blocked_since: i64,
    /// Oldest first.
    pub pending: Vec<PendingPrompt>,
}

/// Which open prompts a settle releases.
#[derive(Clone, Copy, Debug)]
pub enum SettleScope<'a> {
    /// The one prompt the tick matched to a `tool_result`.
    Request(u64),
    /// Every prompt from one agent, on its `SubagentStop`.
    Agent(&'a str),
    /// Every prompt one session raised, on that session's main `Stop` with no
    /// background work.
    Session(&'a str),
}

impl SettleScope<'_> {
    fn covers(self, p: &PendingPrompt) -> bool {
        match self {
            SettleScope::Request(r) => p.request == r,
            SettleScope::Agent(a) => p.prompt.agent_id == a,
            SettleScope::Session(s) => p.prompt.session_id == s,
        }
    }
}

/// What [`AppState::open_subagent_prompt`] did, for its decision log line.
#[derive(Clone, Debug)]
pub struct OpenOutcome {
    pub request: u64,
    /// How many prompts are open on the row now, this one included.
    pub pending: usize,
    /// The main agent's own status under the BLOCK.
    pub base_status: Status,
    /// Whether the dialog changed (only a freshly created row restoring its
    /// history), which is what the caller persists on.
    pub dialog_changed: bool,
}

/// What [`AppState::settle_subagent_prompts`] released.
#[derive(Clone, Debug)]
pub struct SettleOutcome {
    pub settled: Vec<PendingPrompt>,
    pub remaining: usize,
    /// Whether the last prompt settled and the base came back.
    pub released: bool,
    /// The row's visible status after the settle.
    pub status: Status,
}

/// Write the gate's four visible fields, if the row has a gate.
fn show_gate(s: &mut AgentSession) {
    let Some(gate) = s.subagent_gate.as_ref() else { return };
    let Some(newest) = gate.pending.last() else { return };
    let (label, blocked_since) = (newest.prompt.label.clone(), gate.blocked_since);
    s.status = Status::Blocked;
    s.label = label;
    s.waiting_backstop_armed = false;
    s.state_entered_at = blocked_since;
}

/// Run a main-agent write against the row's own state rather than the
/// subagent-prompt overlay on top of it.
///
/// With no gate this is just `f(s)`. Under a gate the base is written into the
/// row, `f` runs on it exactly as it would on an ungated row — so prior-status
/// reads, task boundaries, `status_before_working`, the sticky label and the
/// working-time bank all see the main agent's own state — and whatever `f` left
/// becomes the new base before the BLOCK is shown again. Every writer of the
/// four overlaid fields goes through here: `apply_set`, the watcher's promotion
/// to Working and `revert_cancelled_turn`.
pub(crate) fn with_base<R>(s: &mut AgentSession, f: impl FnOnce(&mut AgentSession) -> R) -> R {
    let Some(mut gate) = s.subagent_gate.take() else { return f(s) };
    gate.base.write_into(s);
    let out = f(s);
    gate.base = BaseState::capture(s);
    s.subagent_gate = Some(gate);
    show_gate(s);
    out
}

impl AgentSession {
    /// The main agent's own status: the base under a pending subagent prompt,
    /// else the row's status. What the sleep holds read, so a workflow running
    /// under a subagent's dialog still counts as work.
    pub fn base_status(&self) -> Status {
        self.subagent_gate.as_ref().map_or(self.status, |g| g.base.status)
    }

    /// Name to show the user in notifications and titles: the custom display
    /// name if one is set, else the chat_id. `display_name` is only populated
    /// off the `CustomNamesStore` (see [`crate::custom_names::CustomNamesStore::apply`]),
    /// so this reads the chat_id anywhere that overlay hasn't been applied.
    pub fn display_label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }

    /// What the row is about right now. For a row the user must act on
    /// (`blocked`/`error`) it is the current `label`, the question or approval
    /// request; otherwise the original task, falling back to `label`. The
    /// fallback order matters because `Stop`→`Done` carries no label, so
    /// `label_policy` keeps the previous one, and a `done` row that was `blocked`
    /// would otherwise still read "needs approval: tool".
    ///
    /// The task is [`shown_task`](Self::shown_task), so a task another agent
    /// began reads as what that agent was asked to do; and every text goes through
    /// `prompt_origin::shown`, so no agent message's envelope is ever what this
    /// returns. The widget row and the Telegram ping both read it, which is what
    /// keeps the two agreeing. A terminal's context line reads
    /// [`shown_task`](Self::shown_task) alone, since the tab title's glyph
    /// already says the row is asking.
    pub fn primary_text(&self) -> Cow<'_, str> {
        let label = || crate::prompt_origin::shown(&self.label);
        let text = match self.status {
            Status::Blocked | Status::Error => label(),
            _ => self.shown_task().or_else(label),
        };
        text.unwrap_or_default()
    }

    /// The task a person gave this row's agent, where it is known:
    /// `delegated_task`, or `original_prompt` when a person typed it. `None`
    /// where the task is another agent's message whose sender's task was never
    /// resolved — the [`message_line`](Self::message_line) standing in for it is
    /// a line that agent wrote, not a task, so it is never reported as this
    /// row's task to anyone else (`http_server::sender_task`,
    /// `prompt_origin::sender_task`).
    pub fn person_task(&self) -> Option<&str> {
        self.delegated_task.as_deref().filter(|t| crate::peer_message::parse_harness_prompt(t).is_none()).or(self.original_prompt.as_deref().filter(|p| crate::peer_message::parse_agent_message(p).is_none() && crate::peer_message::parse_harness_prompt(p).is_none()))
    }

    /// The row's task fit to show: [`person_task`](Self::person_task), else the
    /// [`message_line`](Self::message_line) standing in for it, else what
    /// `prompt_origin::shown` makes of `original_prompt` — an envelope persisted
    /// before `message_line` existed, or synced from a peer predating it, shows
    /// an excerpt of its message there, never the envelope.
    pub fn shown_task(&self) -> Option<Cow<'_, str>> {
        self.person_task().map(Cow::Borrowed).or_else(|| self.message_line.as_deref().map(Cow::Borrowed)).or_else(|| self.original_prompt.as_deref().and_then(crate::prompt_origin::shown))
    }

    /// The text of the widget row's task line, the line under the name, which
    /// `SessionItem.svelte` draws unless `compact_mode` hides it. `None` where the
    /// row has no text for it.
    ///
    /// [`primary_text`](Self::primary_text) where it is not empty; otherwise the
    /// most recent task in the dialog, so a row with history never goes blank and
    /// its history stays one click away. The task is the last task-start entry;
    /// failing that the last user prompt longer than four UTF-16 units, so an
    /// approval like "ok" does not read as a task; then any user prompt; then any
    /// non-separator entry. Prompts and entries that are blank are skipped.
    ///
    /// "Blank" and the four-unit count are measured after `str::trim`, which
    /// strips Rust's `White_Space` set, so U+FEFF is text and U+0085 is not.
    ///
    /// Every entry is read as `prompt_origin::shown` gives it, so a past task
    /// that was another agent's message shows as that message's first line — the
    /// dialog keeps no record of whose task stood behind it, so the sender's own
    /// task that `delegated_task` carries for the current one is not available
    /// here.
    pub fn row_line(&self) -> Option<RowLine> {
        let primary = self.primary_text();
        if !primary.is_empty() {
            return Some(RowLine::Current(primary.into_owned()));
        }
        fn shown(e: &DialogEntry) -> Cow<'_, str> {
            crate::prompt_origin::shown(&e.text).unwrap_or_default()
        }
        let past = match self.dialog.iter().rev().find(|e| e.task_start) {
            Some(task) => Some(task),
            None => {
                let blank = |e: &&DialogEntry| shown(e).trim().is_empty();
                let users = || self.dialog.iter().rev().filter(|e| e.role == DialogRole::User).filter(|e| !blank(e));
                let substantive = users().find(|e| shown(e).trim().encode_utf16().count() > 4);
                let any = || self.dialog.iter().rev().find(|e| e.role != DialogRole::Separator && !blank(e));
                substantive.or_else(|| users().next()).or_else(any)
            }
        };
        past.map(shown).filter(|t| !t.is_empty()).map(|t| RowLine::Past(t.into_owned()))
    }

    /// Every task-start entry in the dialog, oldest first, as the row's hover
    /// tooltip lists them. A person's prompt is its text as typed, newlines and
    /// all, since the tooltip has room to wrap it. Another agent's message reads
    /// as `prompt_origin::shown` gives it, the first line of the message, and
    /// the one that began the current task reads as the task line does
    /// ([`shown_task`](Self::shown_task)), the sender's own task where it was
    /// resolved. A message with no text to show is left out, as is a prompt
    /// Claude Code submitted on its own account, which `shown` gives no text.
    ///
    /// The current task is the entry whose prompt `original_prompt` holds,
    /// compared as `clean_prompt` stored it; an older message of identical text
    /// would read the same either way.
    pub fn task_lines(&self) -> Vec<TaskLine> {
        let current = |e: &DialogEntry| self.original_prompt.as_deref() == Some(crate::adapters::claude::clean_prompt(&e.text).as_str());
        let text = |e: &DialogEntry| match crate::peer_message::parse_agent_message(&e.text) {
            Some(_) if current(e) => self.shown_task().map(Cow::into_owned),
            _ => crate::prompt_origin::shown(&e.text).map(Cow::into_owned),
        };
        self.dialog.iter().filter(|e| e.task_start).filter_map(|e| text(e).filter(|t| !t.trim().is_empty()).map(|text| TaskLine { at: e.timestamp, text })).collect()
    }

    /// The moment this row last produced something the user may not have seen:
    /// the later of the current state's start and the newest assistant text on
    /// the row.
    ///
    /// Both terms move forward on their own, which is what makes `attended_at`
    /// self-falsifying — there is no attention-clearing code anywhere in the hook
    /// path, the watcher, the reaper or the sync ingest, and none is needed.
    ///
    /// The second term is not redundant with the first. `Stop` settles a row
    /// `Done` *before* Claude Code flushes the final reply to JSONL, and the
    /// watcher's `apply_text_entries` bumps only `updated`, never
    /// `state_entered_at` — so a read that happened in that gap would otherwise
    /// count as having seen text that hadn't arrived yet.
    pub(crate) fn content_at(&self) -> i64 {
        let newest_assistant = self
            .dialog
            .iter()
            .rev()
            .find(|e| e.role == DialogRole::Assistant)
            .map(|e| e.timestamp)
            .unwrap_or(0);
        self.state_entered_at.max(newest_assistant)
    }

    /// Whether this row is finished-and-unread, finished-and-read, or not asking
    /// ([`Attention`]).
    ///
    /// Scoped to `Done` in this first cut: it matches what was asked for ("an
    /// agent that have finished the work"), and it keeps this decoration disjoint
    /// from `SessionItem.svelte`'s pulse, which fires on `blocked || error`, so no
    /// row can say "wants attention" and "already seen" at once.
    /// Record that this device has seen whatever [`content_at`](Self::content_at)
    /// now holds, where it has risen above `was` — `None` meaning the row is new
    /// here, so now is when all of it arrived.
    ///
    /// The rule lives beside the field rather than at its two call sites
    /// (`sync::ingest` for the `state_entered_at` term, `sync::store_pulled_dialog`
    /// for the dialog one), because what counts as an arrival is one decision and
    /// the gate reading it cannot tell which path stamped. A third writer is the
    /// drift this exists to stop: a merge that only replaces a reply in place, or
    /// a push that moves nothing, must leave the instant alone, or every heartbeat
    /// would read as fresh content and the gate would stop refusing anything.
    pub fn note_content_arrival(&mut self, was: Option<i64>, now_ms: i64) {
        if was.is_none_or(|before| self.content_at() > before) {
            self.content_seen_at = Some(now_ms);
        }
    }

    pub fn attention(&self) -> Attention {
        if self.status != Status::Done {
            return Attention::Moot;
        }
        match self.attended_at {
            Some(at) if at >= self.content_at() => Attention::Seen,
            _ => Attention::Pending,
        }
    }
}

/// What a row knows that bears on whether a `/pull` run may leave it CLEAN.
/// Read as a set by [`AppState::clean_claim_facts`] and judged by the pure
/// `http_server::pull_declared_clean`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanClaimFacts {
    /// A `/pull` run has posted `POST /api/session-clean` during this turn.
    pub claimed: bool,
    /// This turn was begun by a relayed peer message.
    pub turn_from_relay: bool,
    /// What the row was immediately before this turn began.
    pub status_before_working: Status,
}

#[derive(Clone, Debug)]
pub struct SetInput {
    pub id: String,
    pub status: Status,
    pub label: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub dialog_entry: Option<PendingDialogEntry>,
    /// Set by the `Stop` adapter when it classifies `Waiting`: `true` when a
    /// silently-killable (`shell`) background task holds the WAIT, arming the
    /// `waiting_settle` backstop. `false` for every other event/status.
    pub waiting_backstop_armed: bool,
    /// Who began the turn this event belongs to — `Some(true)` when a relayed
    /// peer message did, `Some(false)` when a human typed it, and `None` from
    /// every event that says nothing either way.
    ///
    /// Set to `Some` by the `UserPromptSubmit` adapter alone, which is the only
    /// event that *is* a prompt arriving. The three-state form is what keeps the
    /// question separate from the status: `log_watcher`'s promote also sets
    /// `Working`, and reading relay-ness off the status would let a promotion
    /// mid-turn silently rewrite who started it.
    ///
    /// `apply_set` acts on `Some` regardless of the transition, unlike
    /// `status_before_working`, which is captured only on a real entry into
    /// `Working`. The two differ because they answer different questions: that
    /// field is where an Esc-cancel reverts to, so it must survive a
    /// `Working` → `Working` prompt, while this one is about the turn now in
    /// flight and has to follow every prompt. Conflating them left a claim from an
    /// unsettled turn redeemable by the next one — caught by
    /// `a_new_turn_revokes_an_unredeemed_clean_claim`.
    pub turn_from_relay: Option<bool>,
    /// For a prompt another agent wrote, its sender's own task where that was
    /// resolved — settled by `http_server` through `prompt_origin::for_arrival`
    /// before the event is applied. `None` for a prompt a person typed and for
    /// every other event. Becomes `AgentSession::delegated_task` exactly when
    /// `label` becomes `original_prompt` (`label_policy::select`).
    pub delegated_task: Option<String>,
    /// For a prompt another agent wrote whose sender's task was not resolved,
    /// the message's first line; becomes `AgentSession::message_line` the same
    /// way.
    pub message_line: Option<String>,
    /// For a prompt another agent wrote, whether it answers a message this
    /// row's agent sent (`AgentMessage::is_reply`); `None` for a prompt a person
    /// typed and for every other event. Set by `http_server` from the adapter's
    /// own parse of the raw prompt, beside `delegated_task`, so `apply_set`
    /// judges the exchange on that one reading rather than parsing `label` again.
    pub message_is_reply: Option<bool>,
}

/// True when `label` (after trim, case-insensitive) matches one of the
/// configured continuation phrases. Used by `apply_set` to suppress a
/// task boundary so a "go" / "continue" / "proceed" reply after a Done
/// status doesn't reset `original_prompt` and the working timer.
fn is_continuation_prompt(label: &str, continuation_prompts: &[String]) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return false;
    }
    continuation_prompts
        .iter()
        .any(|p| p.trim().eq_ignore_ascii_case(trimmed))
}

/// Sessions pushed by one peer dashboard. The in-memory set is repopulated
/// by the peer's next push after a restart; accumulated dialogs are backed
/// by `remote_history` on disk and re-seeded at ingest. Kept separate from
/// `AppState::sessions` so every local-session consumer (`apply_set`,
/// `prompt_history`, `notifications`, `terminal_title`, `log_watcher`) stays
/// remote-blind by construction instead of by per-call filtering.
#[derive(Clone, Debug)]
pub struct RemoteDevice {
    /// Already namespaced ("{device}/{raw_id}") and origin-stamped.
    pub sessions: Vec<AgentSession>,
    /// Receiver-clock ms of the last push from this device — TTL reaping.
    pub last_seen: i64,
    /// Base URL for catch-up dialog fetches, derived from the push's socket
    /// peer IP + advertised listen_port (e.g. "http://100.1.2.3:9078").
    pub origin_addr: String,
    /// The device's live sessions as Claude Code's own registry sees them, from
    /// the last push. `None` = that device gave no registry answer (unreadable
    /// there, or a build predating the field), which the roster reports as such
    /// rather than as an empty machine.
    ///
    /// Kept as the raw wire rows rather than merged into `sessions`: the
    /// registry knows `idle`/`busy` only, which cannot express `blocked`,
    /// `waiting` or `error`, so folding them in would require inventing a
    /// `Status` no record supports.
    pub registry_sessions: Option<Vec<crate::sync::RegistrySync>>,
    /// How this device's claimed name stood up to Tailscale on its last push.
    ///
    /// Stored rather than recomputed on read, because it is a fact about a
    /// *connection that happened* — the source address it arrived from — and
    /// nothing on the read path has that address. Recomputing it later would be
    /// the "record the outcome, don't recompute the decision" mistake: a
    /// predicate over config cannot observe which socket the push came in on.
    ///
    /// Never `Mismatch` here: a mismatched push is refused and never ingested.
    pub identity: crate::tailnet::Attestation,
}

impl AppState {
    /// Each known device's last-pushed registry rows, or `None` where that
    /// device gave no answer. A sibling of [`Self::remote_last_seen`] and
    /// separate for the same reason: the roster needs it while holding no lock
    /// on `remote`, and pairing it with the freshness map is the caller's job.
    pub fn remote_registry(&self) -> BTreeMap<String, Option<Vec<crate::sync::RegistrySync>>> {
        self.remote.lock().unwrap().iter().map(|(d, dev)| (d.clone(), dev.registry_sessions.clone())).collect()
    }

    /// Each known device's identity standing from its last push.
    pub fn remote_identity(&self) -> BTreeMap<String, crate::tailnet::Attestation> {
        self.remote.lock().unwrap().iter().map(|(d, dev)| (d.clone(), dev.identity)).collect()
    }
}

#[derive(Default)]
pub struct AppState {
    /// Sessions running on this machine — the only set the hook/watcher
    /// pipeline, persistence, and notifications ever touch.
    pub sessions: Mutex<Vec<AgentSession>>,
    /// Sessions synced from peer dashboards, keyed by device name. BTreeMap
    /// so the emit-time merge produces a stable row order across emits.
    pub remote: Mutex<BTreeMap<String, RemoteDevice>>,
    /// Ticket counter behind [`AppState::snapshot_versioned`]. Starts at 0 so
    /// the first snapshot is 1 and 0 can mean "no snapshot", which is what a
    /// consumer compares against before it has applied anything.
    snapshot_seq: AtomicU64,
    /// Counter behind [`PendingPrompt::request`], advanced inside the
    /// `sessions` lock like `snapshot_seq`. Starts at 0 so the first request
    /// is 1.
    next_prompt_request: AtomicU64,
}

/// Append a session-boundary separator to a session's dialog in place. Returns
/// whether one was added — skipped when the dialog is empty or already ends with
/// a separator. Shared by [`AppState::mark_session_boundary`] and
/// [`AppState::take_session`] so the boundary rule lives in exactly one place.
fn append_boundary(session: &mut AgentSession, kind: BoundaryKind, now_ms: i64) -> bool {
    if session.dialog.is_empty() {
        return false;
    }
    if session.dialog.last().is_some_and(|e| e.role == DialogRole::Separator) {
        return false;
    }
    session.dialog.push(DialogEntry {
        role: DialogRole::Separator,
        text: String::new(),
        timestamp: now_ms,
        status: Status::Done,
        task_start: false,
        boundary: Some(kind),
    });
    session.updated = now_ms;
    true
}

/// Whether a restored conversation ends at a `/clear` — the only boundary that
/// licenses [`Status::Idle`] on the `SessionStart` that follows it.
///
/// This is deliberately not "ends with a separator". That weaker test reads
/// `true` after a compaction and after every ordinary exit, and since sessions
/// here are started with `--continue`, it would declare nearly every session on
/// the machine clean at start-up. An untagged separator from an older build
/// answers `false` for the same reason.
pub(crate) fn ends_with_clear_boundary(dialog: &[DialogEntry]) -> bool {
    dialog.last().is_some_and(|e| e.role == DialogRole::Separator && e.boundary == Some(BoundaryKind::Clear))
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AgentSession> {
        self.snapshot_versioned().1
    }

    /// The local rows, paired with a ticket saying how recent they are relative
    /// to every other snapshot this process has taken.
    ///
    /// The ticket is minted **inside** the `sessions` lock, and that is the whole
    /// of it: every mutation takes the same lock, so lock order is content order,
    /// and a higher ticket therefore cannot describe an older set of rows. Minting
    /// it after the clone instead would order snapshots by when they finished
    /// copying, which a slow clone reverses — and the clone is the expensive part
    /// (a `Vec<AgentSession>` carrying every dialog, megabytes in practice).
    ///
    /// It exists because a snapshot and the thing built from it are not taken and
    /// published atomically: `commands::emit_sessions_updated` reads here and then
    /// races other emits for the locks downstream, so the emit holding the *older*
    /// snapshot could publish last. Where that publication is corrected by the
    /// next emit, the reordering costs nothing and this is ignorable. Where it is
    /// not — a terminal tab title outlives this process, so a wrong glyph sits
    /// there until some later emit happens to move it, measured at three minutes
    /// on 2026-09-18 — the consumer compares this ticket against the newest it has
    /// applied and stands down when its own is older. See `terminal_title::sync`.
    pub fn snapshot_versioned(&self) -> (u64, Vec<AgentSession>) {
        let sessions = self.sessions.lock().unwrap();
        // Relaxed is enough: the ordering this establishes is the mutex's, not
        // the atomic's — the counter is only ever touched while holding it.
        let seq = self.snapshot_seq.fetch_add(1, Ordering::Relaxed) + 1;
        (seq, sessions.clone())
    }

    /// Flattened snapshot of all remote-device sessions, for the emit-time
    /// merge in `commands::resolved_snapshot`.
    pub fn remote_snapshot(&self) -> Vec<AgentSession> {
        self.remote.lock().unwrap().values().flat_map(|d| d.sessions.iter().cloned()).collect()
    }

    /// Device name → the receiver-clock ms of that device's last push. Exists
    /// because freshness lives on the *device*, not on [`AgentSession`], so a
    /// caller holding a merged session list has no way to reach it — the
    /// `/api/agents` roster needs both and gets them from two accessors.
    ///
    /// It is not an optimization: the roster's other half is `resolved_snapshot`,
    /// which does clone every session including its dialog. Saying otherwise
    /// here would misdescribe the only path that calls this.
    pub fn remote_last_seen(&self) -> BTreeMap<String, i64> {
        self.remote.lock().unwrap().iter().map(|(device, d)| (device.clone(), d.last_seen)).collect()
    }

    /// Drop remote devices not heard from within `ttl_ms`. Returns `true`
    /// when anything was dropped (caller re-emits).
    pub fn reap_remote(&self, now_ms: i64, ttl_ms: i64) -> bool {
        let mut remote = self.remote.lock().unwrap();
        let before = remote.len();
        remote.retain(|_, d| now_ms - d.last_seen <= ttl_ms);
        remote.len() != before
    }

    /// Returns `true` when the session's dialog was modified (caller should
    /// persist). The `restored` parameter is used only when creating a new
    /// session to pre-populate dialog + original_prompt + task_started_at
    /// from the persistence store.
    pub fn apply_set(
        &self,
        input: SetInput,
        now_ms: i64,
        continuation_prompts: &[String],
        restored: Option<PersistedSession>,
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let dialog_entry = input.dialog_entry.clone();
        if let Some(existing) = sessions.iter_mut().find(|s| s.id == input.id) {
            // A pending subagent prompt overlays the row, and this is the main
            // agent's own event, so everything below reads and writes the base
            // underneath it (see `with_base`); the row stays BLOCK when gated.
            let gated = existing.subagent_gate.is_some();
            with_base(existing, |existing| {
                let prior = existing.status;

                let raw_task_boundary = matches!(
                    prior,
                    Status::Done | Status::Idle | Status::Working | Status::Waiting
                ) && input.status == Status::Working;
                let is_continuation = raw_task_boundary
                    && input
                        .label
                        .as_deref()
                        .is_some_and(|l| is_continuation_prompt(l, continuation_prompts));
                // A prompt that names no task — the adapter gives no label to an
                // empty prompt, a `<task-notification>`, a subagent's hand-back or
                // a cross-session notice — runs a turn of the task already there,
                // so its timer and its dialog's task marks stay as they are. A
                // prompt is what `turn_from_relay` being `Some` means.
                let prompt_names_no_task = input.label.is_none() && input.turn_from_relay.is_some();
                // Another agent's message belongs to the exchange already under
                // way, not to a new task, when it arrives while this row's turn is
                // still running, or when it is a reply to a message this row's
                // agent sent — which usually lands after that turn ended, since
                // nothing polls for it. Any other message on a finished row may
                // start a task. Only the relay marks a reply, so a `SendMessage`
                // reply on a finished row still starts one.
                let message_in_exchange = input.message_is_reply.is_some_and(|reply| matches!(prior, Status::Working | Status::Waiting) || reply);
                let task_boundary = raw_task_boundary && !is_continuation && !prompt_names_no_task && !message_in_exchange;

                if prior == Status::Working && input.status != Status::Working {
                    let delta = (now_ms - existing.state_entered_at).max(0) as u64;
                    existing.working_accumulated_ms = existing.working_accumulated_ms.saturating_add(delta);
                }

                // Remember where to revert if this turn is cancelled with Esc. Only
                // capture on a real entry into Working (not Working → Working), so
                // the snapshot is always a genuine pre-prompt status — typically the
                // `Blocked` of a question the user is mid-answer to.
                if input.status == Status::Working && prior != Status::Working {
                    existing.status_before_working = prior;
                }

                // A prompt arriving is what records who began the turn, and what
                // revokes any clean claim still outstanding. Keyed on the event
                // being a prompt (`Some`) rather than on the transition, because a
                // second prompt before any `Stop` is `Working` → `Working`: gating
                // this on the transition above left the first turn's claim and its
                // relay flag in place for the next turn to redeem.
                //
                // `/pull` posts from inside the turn it reports on, so a claim
                // still here when another prompt lands is one whose `Stop` never
                // came — the turn went on to do other work, or was cancelled.
                // Either way it no longer describes anything about to end, which
                // is the revocation rule falling out of the ordering rather than
                // needing a timer.
                if let Some(relay) = input.turn_from_relay {
                    existing.turn_from_relay = relay;
                    existing.clean_claim_at = None;
                }

                let crate::label_policy::Selected { label: new_label, original_prompt: new_original_prompt, delegated_task: new_delegated_task, message_line: new_message_line } =
                    crate::label_policy::select(Some(&*existing), &input, task_boundary);

                if task_boundary {
                    existing.working_accumulated_ms = 0;
                }

                if prior != input.status || task_boundary {
                    existing.state_entered_at = now_ms;
                }

                tracing::debug!(
                    id = %input.id,
                    decision = "apply_set",
                    path = "existing",
                    prior_status = ?prior,
                    new_status = ?input.status,
                    gated,
                    task_boundary,
                    continuation_suppressed = is_continuation,
                    no_task_prompt = prompt_names_no_task,
                    message_in_exchange,
                    input_label = ?input.label,
                    prior_original_prompt = ?existing.original_prompt,
                    new_label = %new_label,
                    new_original_prompt = ?new_original_prompt,
                    "apply_set"
                );

                if task_boundary
                    && new_original_prompt.is_some()
                    && new_original_prompt != existing.original_prompt
                {
                    existing.task_started_at = now_ms;
                }

                existing.status = input.status;
                existing.waiting_backstop_armed = input.waiting_backstop_armed;
                existing.label = new_label;
                existing.original_prompt = new_original_prompt;
                existing.delegated_task = new_delegated_task;
                existing.message_line = new_message_line;
                if let Some(src) = input.source {
                    existing.source = src;
                }
                if input.model.is_some() {
                    existing.model = input.model;
                }
                if input.input_tokens.is_some() {
                    existing.input_tokens = input.input_tokens;
                }
                existing.updated = now_ms;

                if let Some(pending) = dialog_entry {
                    let task_start = pending.role == DialogRole::User && task_boundary;
                    existing.dialog.push(DialogEntry {
                        boundary: None,
                        role: pending.role,
                        text: pending.text,
                        timestamp: now_ms,
                        status: existing.status,
                        task_start,
                    });
                    return true;
                }
                false
            })
        } else {
            let selected = crate::label_policy::select(None, &input, false);
            tracing::debug!(
                id = %input.id,
                decision = "apply_set",
                path = "new",
                new_status = ?input.status,
                input_label = ?input.label,
                new_label = %selected.label,
                new_original_prompt = ?selected.original_prompt,
                "apply_set"
            );
            let (session, seeded) = new_session(input, selected, dialog_entry, now_ms, now_ms, restored);
            sessions.push(session);
            seeded
        }
    }

    /// Create a row for a session this process has *not* heard from, or do
    /// nothing if one already exists.
    ///
    /// Separate from [`apply_set`](Self::apply_set) for one reason, and it is a
    /// correctness one rather than tidiness: `apply_set`'s existing-row branch
    /// overwrites `status` and resets `state_entered_at` unconditionally, so a
    /// restore racing a live hook event — the axum server is already accepting
    /// them by the time this runs — would stamp a session that is genuinely
    /// `Working` back to whatever its tab said before the restart. The check and
    /// the insert therefore happen under one lock, and the answer to "it is
    /// already there" is to leave it entirely alone.
    ///
    /// `state_entered_at` is passed in rather than taken from `now_ms` because
    /// the two are different facts here: the row is being *written* now, but its
    /// status began when the agent last acted, which is what every age-based
    /// reader — the widget's elapsed clock, `/api/agents`' `status_age_ms`, the
    /// notifier's time-in-state — needs to be told. `updated` stays `now_ms`,
    /// since that is a fact about this process's bookkeeping.
    ///
    /// `read` restores the one fact that lives nowhere but the tab. `attended_at`
    /// is `#[serde(skip)]` and never persisted, so a ⚪ glyph is the only surviving
    /// record that the user already looked at this row. It is stamped to the
    /// row's own `content_at` rather than to `now_ms`: the claim being restored is
    /// "seen as of everything this row had produced when its tab was last
    /// written", and `now_ms` would additionally vouch for anything that arrives
    /// between here and the next emit.
    ///
    /// Returns whether a row was created.
    pub fn restore_row(&self, input: SetInput, read: bool, state_entered_at: i64, now_ms: i64, restored: Option<PersistedSession>) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.iter().any(|s| s.id == input.id) {
            return false;
        }
        let selected = crate::label_policy::select(None, &input, false);
        let (mut session, _) = new_session(input, selected, None, state_entered_at, now_ms, restored);
        if read {
            session.attended_at = Some(session.content_at());
        }
        sessions.push(session);
        true
    }

    /// Open a subagent's permission prompt on its row: the row reads `Blocked`
    /// until the prompt settles, over a base the main agent keeps writing.
    ///
    /// Separate from [`apply_set`](Self::apply_set) because this is not the
    /// main agent's event: the task boundary, `status_before_working`,
    /// `original_prompt`, the dialog and the working-time bank all describe the
    /// main agent and are left alone. The request id is minted and the prompt
    /// recorded under the same lock that changes the status, so a settle can
    /// never see one without the other.
    ///
    /// On an existing row the first prompt captures the base; later ones join
    /// without re-capturing it, which is what lets two agents' prompts settle in
    /// either order. A row this creates has nothing of the main agent's to
    /// capture, so its base is a `Done` stamped now — the neutral sink, for the
    /// reason the branch that writes it gives.
    pub fn open_subagent_prompt(&self, input: SetInput, prompt: SubagentPromptRequest, now_ms: i64, restored: Option<PersistedSession>) -> OpenOutcome {
        let mut sessions = self.sessions.lock().unwrap();
        // Relaxed for the reason `snapshot_versioned` gives: the ordering is the mutex's.
        let request = self.next_prompt_request.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = PendingPrompt { request, requested_at: now_ms, prompt };
        if let Some(s) = sessions.iter_mut().find(|s| s.id == input.id) {
            let mut gate = s.subagent_gate.take().unwrap_or_else(|| SubagentGate {
                base: BaseState::capture(s),
                blocked_since: if s.status == Status::Blocked { s.state_entered_at } else { now_ms },
                pending: Vec::new(),
            });
            gate.pending.push(entry);
            let (pending, base_status) = (gate.pending.len(), gate.base.status);
            s.subagent_gate = Some(gate);
            show_gate(s);
            s.updated = now_ms;
            return OpenOutcome { request, pending, base_status, dialog_changed: false };
        }
        let selected = crate::label_policy::select(None, &input, false);
        let (mut session, seeded) = new_session(input, selected, None, now_ms, now_ms, restored);
        // `Done`, not `Idle`. All this row's existence proves is that some main
        // agent launched a subagent that asked for permission — nothing about
        // whether the user has anything to come back to, so the neutral sink is
        // the only honest base. `settle_subagent_prompts` writes this back
        // verbatim when the dialog releases, so it is what the row then shows.
        let base = BaseState { status: Status::Done, label: String::new(), waiting_backstop_armed: false, state_entered_at: now_ms };
        session.subagent_gate = Some(SubagentGate { base, blocked_since: now_ms, pending: vec![entry] });
        show_gate(&mut session);
        sessions.push(session);
        OpenOutcome { request, pending: 1, base_status: Status::Done, dialog_changed: seeded }
    }

    /// Settle the open subagent prompts `scope` covers on row `id`.
    ///
    /// When none remain, the base is written back exactly as stored — status,
    /// label, backstop and clock — so the row reads as if the prompt had never
    /// opened over whatever the main agent did meanwhile. Otherwise the row stays
    /// BLOCK under the newest remaining prompt's label. Never goes through
    /// `apply_set`: releasing a prompt is not a status the main agent entered,
    /// so there is no task boundary to detect and no working time to bank.
    ///
    /// `None` when the row is gone or nothing matched, and then `updated` is
    /// left alone, so a settle racing a `/clear` or a second exit is a no-op.
    pub fn settle_subagent_prompts(&self, id: &str, scope: SettleScope, now_ms: i64) -> Option<SettleOutcome> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.iter_mut().find(|s| s.id == id)?;
        let gate = s.subagent_gate.as_mut()?;
        let (settled, remaining): (Vec<PendingPrompt>, Vec<PendingPrompt>) = std::mem::take(&mut gate.pending).into_iter().partition(|p| scope.covers(p));
        let left = remaining.len();
        gate.pending = remaining;
        if settled.is_empty() {
            return None;
        }
        let released = left == 0;
        if released {
            if let Some(gate) = s.subagent_gate.take() {
                gate.base.write_into(s);
            }
        } else {
            show_gate(s);
        }
        s.updated = now_ms;
        Some(SettleOutcome { settled, remaining: left, released, status: s.status })
    }

    /// Every open subagent prompt with the row it sits on, for the tick that
    /// reads the agents' transcripts. Clones the prompts only, never a dialog.
    pub fn pending_subagent_prompts(&self) -> Vec<(String, PendingPrompt)> {
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().flat_map(|s| s.subagent_gate.iter().flat_map(|g| g.pending.iter().map(|p| (s.id.clone(), p.clone())))).collect()
    }
}

/// Build a brand-new row, shared by [`AppState::apply_set`]'s new-session branch
/// and [`AppState::restore_row`].
///
/// One constructor rather than two, because the fields that must be right on a
/// fresh row — the separator-terminated dialog rule, `status_before_working`,
/// the `Canary::Off` / `origin: None` / `attended_at: None` triple — are exactly
/// the ones a second copy would forget. Returns the row and whether it carries
/// any content (a pushed entry or a restored dialog), which is what `apply_set`
/// reports as "the dialog changed".
fn new_session(
    input: SetInput,
    selected: crate::label_policy::Selected,
    dialog_entry: Option<PendingDialogEntry>,
    state_entered_at: i64,
    now_ms: i64,
    restored: Option<PersistedSession>,
) -> (AgentSession, bool) {
    let r = restored.unwrap_or_default();
    // A restored dialog ending in a separator means a boundary of some kind was
    // the last thing on the row, so no task is in flight. Don't resurrect the
    // pre-boundary task's prompt/timer onto the fresh row. Keep the dialog for
    // history continuity but start the row's active-task state clean. A prompt
    // the event itself carries (a Working prompt arriving with this same event)
    // still takes precedence and starts a real task, and brings its own
    // `delegated_task` and `message_line` with it rather than inheriting the
    // restored ones.
    //
    // Any boundary suppresses the prompt, because none of them leaves a task
    // running. Whether the row is also CLEAN is a narrower question answered by
    // `ends_with_clear_boundary` on the `SessionStart` path — only a `/clear`
    // counts there, while a compaction or an ordinary exit leaves a
    // conversation somebody may want back.
    let ended_at_boundary = r.dialog.last().is_some_and(|e| e.role == DialogRole::Separator);
    let restored_task = if ended_at_boundary { None } else { r.original_prompt.map(|p| (p, r.delegated_task, r.message_line)) };
    let restored_task_started_at = if ended_at_boundary { 0 } else { r.task_started_at };
    let crate::label_policy::Selected { label, original_prompt: event_prompt, delegated_task: event_delegated, message_line: event_line } = selected;
    let (original_prompt, delegated_task, message_line) = match event_prompt {
        Some(p) => (Some(p), event_delegated, event_line),
        None => restored_task.map_or((None, None, None), |(p, d, l)| (Some(p), d, l)),
    };
    let task_started_at = if original_prompt.is_some() && restored_task_started_at == 0 { now_ms } else { restored_task_started_at };
    let mut dialog = r.dialog;

    let has_new_entry = if let Some(pending) = dialog_entry {
        // A prompt naming no task starts none, as in `apply_set`.
        let task_start = pending.role == DialogRole::User && input.label.is_some();
        dialog.push(DialogEntry { role: pending.role, text: pending.text, timestamp: now_ms, status: input.status, task_start, boundary: None });
        true
    } else {
        false
    };

    let dialog_restored = !dialog.is_empty();
    let session = AgentSession {
        id: input.id,
        status: input.status,
        // `Done`, not `Idle`: this is where an Esc-cancelled *first* turn lands,
        // and a row whose only turn was abandoned has not been established as
        // having nothing to come back to — the cancelled turn may well have
        // edited files.
        status_before_working: Status::Done,
        label,
        original_prompt,
        delegated_task,
        message_line,
        task_started_at,
        dialog,
        source: input.source.unwrap_or_else(|| "claude-code".to_string()),
        model: input.model,
        input_tokens: input.input_tokens,
        updated: now_ms,
        state_entered_at,
        working_accumulated_ms: 0,
        waiting_backstop_armed: input.waiting_backstop_armed,
        display_name: None,
        origin: None,
        instruction_drift: false,
        terminal_stale_at: None,
        canary: Canary::Off,
        attended_at: None,
        content_seen_at: None,
        origin_label: None,
        // Carried where the event that creates the row is itself a prompt. The
        // row is still refused a CLEAN settle, because `status_before_working`
        // on a row this process has never seen before cannot say it was clean —
        // but the fact that is knowable is recorded rather than flattened.
        turn_from_relay: input.turn_from_relay.unwrap_or(false),
        clean_claim_at: None,
        read: false,
        name_shared_by: None,
        row_line: None,
        task_lines: Vec::new(),
        subagent_gate: None,
    };
    (session, has_new_entry || dialog_restored)
}

impl AppState {

    /// Remove a session and return it, after appending a session boundary to its
    /// dialog (so a restored copy ends with a separator, exactly like `/clear`).
    /// When `expect_updated` is `Some`, the removal is aborted (returns `None`)
    /// if the row's `updated` no longer matches — used by the liveness reaper to
    /// avoid deleting a row that received a new event between observation and
    /// removal. The check, boundary append, and removal all happen under one
    /// lock, so it is atomic against a concurrent hook event.
    pub fn take_session(&self, id: &str, expect_updated: Option<i64>, kind: BoundaryKind, now_ms: i64) -> Option<AgentSession> {
        let mut sessions = self.sessions.lock().unwrap();
        let pos = sessions.iter().position(|s| s.id == id)?;
        if let Some(expected) = expect_updated {
            if sessions[pos].updated != expected {
                return None;
            }
        }
        append_boundary(&mut sessions[pos], kind, now_ms);
        Some(sessions.remove(pos))
    }

    /// Revert a `Working` session whose turn was cancelled with Esc back to the
    /// status it held *before* the turn started (`status_before_working`),
    /// rather than blanket-`Idle`. Called by the transcript watcher on the
    /// "[Request interrupted by user]" marker — an Esc emits no lifecycle hook,
    /// so without this the row would stay `Working` forever (and the watcher's
    /// own `infer_state` would
    /// otherwise re-promote the marker as user input). The cancelled turn
    /// produced nothing, so the row should look as if the prompt never landed:
    /// a reply aborted mid-question reverts to `Blocked`, so the user's real
    /// answer is an approval-cycle reply (no task boundary) instead of a fresh
    /// task that clobbers `original_prompt`. A turn cancelled while `Blocked`
    /// is reverted the same way: Esc on an `AskUserQuestion`, a plan approval
    /// or a permission dialog writes the marker with no hook, and the dialog it
    /// closes is the only thing holding the row BLOCK — a `Stop`'s `Blocked`
    /// ends its turn, so no marker can follow it before the next prompt moves
    /// the row to `Working`. No-op for any other status, so a turn that already
    /// settled is left alone. Mirrors `apply_set`'s Working→non-Working
    /// accounting (banks the elapsed run, resets the timer); a `Blocked` row's
    /// run was banked when it left `Working`.
    ///
    /// The Esc is the main agent's, so under a pending subagent prompt it
    /// reverts the base and the row stays BLOCK (see [`with_base`]).
    ///
    /// Returns the base status it reverted to and whether a subagent prompt was
    /// overlaying the row (both for the decision log), or `None` when it was a
    /// no-op because the main agent's turn had already settled.
    pub fn revert_cancelled_turn(&self, id: &str, now_ms: i64) -> Option<(Status, bool)> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.iter_mut().find(|s| s.id == id)?;
        let gated = s.subagent_gate.is_some();
        with_base(s, |s| {
            match s.status {
                Status::Working => {
                    let delta = (now_ms - s.state_entered_at).max(0) as u64;
                    s.working_accumulated_ms = s.working_accumulated_ms.saturating_add(delta);
                }
                Status::Blocked => {}
                _ => return None,
            }
            // A cancelled turn can restore any prior resting state except
            // CLEAN, and that exception is the whole reason this is not a plain
            // assignment. The row a user types into next is very often one they
            // just cleared, so `status_before_working` is `Idle` in the common
            // case — and by the time Esc is pressed the turn may have edited
            // files, which is exactly when a row must not claim there is
            // nothing to come back to. Restoring the *previous* state is right
            // for `Blocked`, where an aborted reply leaves the question
            // standing; for `Idle` the turn itself is the counter-evidence.
            s.status = match s.status_before_working {
                Status::Idle => Status::Done,
                prior => prior,
            };
            s.state_entered_at = now_ms;
            s.updated = now_ms;
            Some((s.status, gated))
        })
    }

    /// Record that a `/pull` run reported leaving nothing worth coming back to.
    ///
    /// Stores only the claim — whether it may become [`Status::Idle`] is
    /// `http_server::pull_declared_clean`'s to decide at the turn's `Stop`, from
    /// this plus the two facts `apply_set` captured about the turn. Returns
    /// `false` when no such row exists, which is the honest answer to a claim for
    /// a session this dashboard is not tracking.
    ///
    /// Deliberately **not** routed through `apply_set`: nothing about the row's
    /// status changes here, so going through the transition machinery would reset
    /// `state_entered_at` and bank working time for a bookkeeping write. It does
    /// not bump `updated` either, for the reason `mark_attended` does not — a
    /// claim is not activity, and `updated` is the compare-and-swap guard the
    /// reaper and the WAIT backstop abort on.
    pub fn record_clean_claim(&self, id: &str, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.iter_mut().find(|s| s.id == id) else { return false };
        s.clean_claim_at = Some(now_ms);
        true
    }

    /// The three facts `pull_declared_clean` needs about a row, read under one
    /// lock so they cannot be sampled at different instants.
    pub fn clean_claim_facts(&self, id: &str) -> Option<CleanClaimFacts> {
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().find(|s| s.id == id).map(|s| CleanClaimFacts {
            claimed: s.clean_claim_at.is_some(),
            turn_from_relay: s.turn_from_relay,
            // The main agent's own status, so a subagent prompt overlaying the
            // row cannot make its pre-turn state unreadable.
            status_before_working: s.status_before_working,
        })
    }

    /// Settle a stale `Waiting` row to `Done` — the backstop for background work
    /// that ended without a signal (a user-killed dev server writes nothing to
    /// the hooks or the transcript, so no `Stop` ever clears the row). Called by
    /// `waiting_settle` once the row has sat in `Waiting` for the grace window.
    /// Guarded by `expect_updated`: if any event bumped `updated` since the tick
    /// observed the row (a follow-up turn, a new prompt), the settle aborts so it
    /// can't clobber a row that just moved on — closing the settle-vs-event race.
    /// No-op unless still `Waiting`. `Waiting` isn't `Working`, so there's no
    /// run-time to bank (mirrors `revert_cancelled_turn`, which does). Returns
    /// true if it acted. A `Waiting` base under a pending subagent prompt reads
    /// `Blocked`, so the status guard refuses it until the prompt settles and
    /// the restored clock resumes the count.
    pub fn settle_stale_waiting(&self, id: &str, expect_updated: i64, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if s.status != Status::Waiting || s.updated != expect_updated {
            return false;
        }
        s.status = Status::Done;
        s.state_entered_at = now_ms;
        s.updated = now_ms;
        true
    }

    /// Flag or clear "this row's terminal tab is showing a stale status".
    ///
    /// A time-based expiry backstop was added here and removed again. It was meant
    /// to bound a flag the detector failed to retract, but the revert that landed
    /// beside it removed the only state in which that could happen — `Report` and
    /// `Clear` are symmetric again — so its one reachable trigger was
    /// `stale_step`'s deliberate `Hold`, the arm that refuses to retract on a
    /// coincidental agreement. It therefore cleared exactly the flags that arm
    /// exists to keep, preferentially in the accidental-rename case this feature
    /// is for. If an unclearable state ever returns, bound it at the point that
    /// knows it is unclearable, not on a timer that cannot tell a held verdict
    /// from a missing one.
    ///
    /// Returns whether anything changed, so the caller only emits and logs on an
    /// edge. Mirrors [`AppState::set_drift`], including leaving `status`
    /// untouched.
    pub fn set_terminal_stale(&self, id: &str, stale: bool, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        // Compare presence, not value: a repeat confirmation must not restart the
        // clock the alert's delay is measured against.
        if s.terminal_stale_at.is_some() == stale {
            return false;
        }
        s.terminal_stale_at = stale.then_some(now_ms);
        s.updated = now_ms;
        true
    }

    /// Clear every stale-tab flag, for when title writing is turned off: with
    /// nothing being written, nothing can be found to disagree with, so a
    /// standing warning would outlive the evidence for it.
    pub fn clear_all_terminal_stale(&self, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let mut changed = false;
        for s in sessions.iter_mut().filter(|s| s.terminal_stale_at.is_some()) {
            s.terminal_stale_at = None;
            s.updated = now_ms;
            changed = true;
        }
        changed
    }

    /// Set (or clear) a session's instruction-drift flag — the canary overlay,
    /// stamped from the `http_server` Stop check when the final assistant message
    /// is (or is no longer) missing this session's adherence marker. Orthogonal to
    /// `status`: it never changes the state, only this flag, so a drifting turn
    /// still reads Done / Blocked / Waiting. Bumps `updated` (so the following
    /// `emit_sessions_updated` fans the change out to every surface) and returns
    /// whether the value actually changed. No-op when the row is gone or already at
    /// `drift`.
    pub fn set_drift(&self, id: &str, drift: bool, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if s.instruction_drift == drift {
            return false;
        }
        s.instruction_drift = drift;
        s.updated = now_ms;
        true
    }

    /// Record that the user was observed attending to `id` at `at_ms`. Orthogonal
    /// to `status`: it never changes the state.
    ///
    /// Deliberately does **not** bump `updated`. Unlike [`Self::set_drift`], this
    /// is a local observation of the user rather than a fact about the agent, and
    /// `updated` is the compare-and-swap guard that `take_session` (the liveness
    /// reaper) and `settle_stale_waiting` abort on — and whose *stability* the
    /// reaper's dead-streak counter requires. Stamping it here would restart that
    /// count every time the user looked at a row.
    ///
    /// Monotonic, so a slow poll answering with an older observation can't walk
    /// the stamp backwards. Returns whether the row's [`AgentSession::attention`]
    /// *verdict* changed — not whether the timestamp moved — so a re-stamp on an
    /// already-`Seen` (or `Moot`) row emits nothing, and the presence sensor
    /// doesn't fire an event, a terminal-title reconcile and a sync poke on every
    /// tick while the user sits typing.
    pub fn mark_attended(&self, id: &str, at_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        let before = s.attention();
        s.attended_at = Some(s.attended_at.map_or(at_ms, |prev| prev.max(at_ms)));
        s.attention() != before
    }

    /// Record that the user attended to a **synced** row here — the SSH or tmux
    /// tab rendering another machine's agent is on this machine, so leaving it
    /// is this machine's observation to make.
    ///
    /// Separate from [`mark_attended`](Self::mark_attended) for two reasons, and
    /// the second is the one that matters.
    ///
    /// It reaches the other store: `sessions` holds local rows and `remote` holds
    /// synced ones, so one function cannot serve both without taking two locks.
    ///
    /// And **the instant it takes is not the one it stamps.** A remote row's
    /// `content_at` is built from timestamps the *origin* minted, so an instant
    /// from this machine's clock cannot be compared against it. What is recorded
    /// is the row's own `content_at` — "seen as of everything this row currently
    /// holds", a value the origin itself produced, the same technique
    /// [`restore_row`](Self::restore_row) uses. Read under the lock that writes
    /// it, so a sync ingest landing between cannot move the watermark out from
    /// under the stamp.
    ///
    /// But stamping that value satisfies [`attention`](AgentSession::attention)
    /// by construction, and that comparison *is* the recency test the local path
    /// relies on — so without a second one here, any observation would credit the
    /// row however old it was, and stale ones are routine rather than exotic
    /// (see [`content_seen_at`](AgentSession::content_seen_at)). The test moves
    /// instead of disappearing: `at_ms` is the observation's instant and
    /// `content_seen_at` is when this device saw that content arrive, both on
    /// this machine's clock, and an observation older than the content it would
    /// vouch for is refused. An unrecorded arrival refuses too — `ingest` writes
    /// one on every push, so not having it means this row has not been heard
    /// about yet rather than that the content is old.
    ///
    /// Erring early falls out twice over. Arrival is necessarily later than
    /// production, so the gate refuses slightly more than strictly necessary; and
    /// this device's dialog copy lags the origin's while a pull is outstanding,
    /// so the watermark recorded is at most the origin's and the rest of the turn
    /// un-reads the row when it lands, exactly as a late local flush does.
    ///
    /// Returns the stamp when the row's verdict changed, for the caller to report
    /// to the origin; `None` when the row is gone, was already read, or the
    /// observation predates the content.
    pub fn mark_remote_attended(&self, id: &str, at_ms: i64) -> Option<i64> {
        let mut remote = self.remote.lock().unwrap();
        let s = remote.values_mut().flat_map(|d| d.sessions.iter_mut()).find(|s| s.id == id)?;
        if at_ms < s.content_seen_at? {
            return None;
        }
        let before = s.attention();
        let at = s.content_at();
        s.attended_at = Some(s.attended_at.map_or(at, |prev| prev.max(at)));
        (s.attention() != before).then_some(at)
    }

    /// A local row's [`AgentSession::content_at`], or `None` when no such row
    /// exists here.
    ///
    /// The ceiling `sync::post_attention` clamps an incoming peer report to.
    /// Read through `AppState` rather than handed the row, because the clamp has
    /// to be against what this machine holds *now*: the value in the report was
    /// minted here, but on a snapshot the reporter pulled, and a bound taken
    /// from anything else would be a bound on a row that is not this one.
    pub fn content_at_of(&self, id: &str) -> Option<i64> {
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().find(|s| s.id == id).map(AgentSession::content_at)
    }

    /// Forget every attention observation — the teardown for
    /// `config.attention_tracking` going false, so every row renders exactly as
    /// it did before the feature existed. Mirrors [`Self::clear_all_drift`].
    ///
    /// A synced row carries two different things and this clears exactly one of
    /// them. Its `attended_at` is an observation *this* machine made — a tab here
    /// rendering that agent over SSH or tmux — so it goes with the local ones.
    /// Its `read` is the origin's verdict, which is not this machine's to erase
    /// and could not be erased anyway, since the origin re-advertises it on its
    /// very next push; `commands::forget_read` is what declines to *draw* it,
    /// on the display path where the same switch already decides the local half.
    ///
    /// The two locks are taken one after the other rather than together, matching
    /// `commands::resolved_snapshot_versioned`, so neither is held across the
    /// other and there is no order to invert.
    pub fn clear_all_attention(&self) -> bool {
        let mut changed = false;
        {
            let mut sessions = self.sessions.lock().unwrap();
            for s in sessions.iter_mut().filter(|s| s.attended_at.is_some()) {
                s.attended_at = None;
                changed = true;
            }
        }
        let mut remote = self.remote.lock().unwrap();
        for s in remote.values_mut().flat_map(|d| d.sessions.iter_mut()).filter(|s| s.attended_at.is_some()) {
            s.attended_at = None;
            changed = true;
        }
        changed
    }

    /// Current confirmed instruction-drift flag for a row (false when the row is
    /// gone). Read by the `http_server` Stop check to report the resulting state
    /// after a `Hold` (deferred) decision leaves the flag untouched.
    pub fn drift_confirmed(&self, id: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions.iter().find(|s| s.id == id).is_some_and(|s| s.instruction_drift)
    }

    /// Clear the instruction-drift flag on every local session — called when the
    /// adherence canary is turned off, so the row badge and terminal-title `⚠`
    /// drop immediately, matching the Telegram reconciler (which dismisses its
    /// pings on the same toggle). Without this, the only writer of the flag lives
    /// behind the feature gate, so a stranded warning would otherwise persist for
    /// the row's life. Bumps `updated` on each changed row and returns whether any
    /// changed (the caller emits only then).
    pub fn clear_all_drift(&self, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let mut changed = false;
        for s in sessions.iter_mut().filter(|s| s.instruction_drift) {
            s.instruction_drift = false;
            s.updated = now_ms;
            changed = true;
        }
        changed
    }

    /// Mark a compaction boundary in the in-memory dialog — a history separator
    /// without ending the session, so the prior task is not resurrected onto the
    /// next turn. `PreCompact` is its only caller; the `/clear` half of the pair
    /// goes through [`AppState::take_session`], which appends its own separator
    /// on the way out and tags it [`BoundaryKind::Clear`].
    ///
    /// The kind matters here rather than being cosmetic: a compaction leaves a
    /// conversation the user still wants, so this boundary must never license
    /// the `Idle` a `/clear` does.
    pub fn mark_session_boundary(&self, id: &str, now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        // A context compaction shrinks the real context, so clear the stale
        // pre-compact token count. `input_tokens` is watcher-only and isn't
        // rewritten until the next turn flushes fresh usage to the transcript —
        // if the user compacts and walks away, context_percent would otherwise
        // stay frozen at the old (high) value and the reconciler would fire a
        // spurious high-context alert. `None` = unknown until the watcher
        // repopulates it from the first post-compact turn.
        let tokens_cleared = session.input_tokens.take().is_some();
        let separator_added = append_boundary(session, BoundaryKind::Compact, now_ms);
        if tokens_cleared && !separator_added {
            session.updated = now_ms;
        }
        tokens_cleared || separator_added
    }

    /// Watcher-driven text capture. Processes transcript text entries in
    /// chronological order. User entries append (with dedup). Assistant
    /// entries replace the last assistant in the current turn (same-turn
    /// update), or append if a user entry separates them (new turn after
    /// an interrupt).
    pub fn apply_text_entries(&self, id: &str, entries: &[(DialogRole, String)], now_ms: i64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        // The watcher only ever yields User/Assistant; separators enter a
        // dialog via mark_session_boundary, not the transcript.
        let incoming: Vec<DialogEntry> = entries
            .iter()
            .filter(|(role, _)| matches!(role, DialogRole::User | DialogRole::Assistant))
            .map(|(role, text)| DialogEntry { role: *role, text: text.clone(), timestamp: now_ms, status: session.status, task_start: false, boundary: None })
            .collect();
        let changed = merge_dialog_entries(&mut session.dialog, &incoming);
        if changed {
            session.updated = now_ms;
        }
        changed
    }
}

/// Merge `incoming` dialog entries (chronological order) into `dialog` with
/// the turn-aware semantics of the transcript watcher, which is its only
/// consumer — a peer's entries arrive through `sync::merge_synced_dialog`,
/// which identifies an entry rather than inferring one. The rules below are
/// therefore about one thing: the watcher re-reads a transcript it has already
/// read, so the same entry reaches this function more than once.
/// - User: append, unless the dialog still *ends* with a user entry of the
///   same text (the re-read of an unanswered prompt) or an identical entry
///   — same timestamp and text — already exists.
/// - Assistant: replace the tail assistant of the current turn in place
///   (the same-turn streaming update), skip when its text already matches,
///   append when a user entry or a separator intervened.
/// - Separator: append, unless the dialog already ends with one (mirrors the
///   mark_session_boundary guard) or the same separator (by timestamp) was
///   already merged. No caller reaches this arm: `apply_text_entries` filters
///   the watcher's entries down to User/Assistant, separators entering a
///   dialog through `mark_session_boundary` instead.
/// Returns `true` when the dialog was modified.
pub fn merge_dialog_entries(dialog: &mut Vec<DialogEntry>, incoming: &[DialogEntry]) -> bool {
    let mut changed = false;
    for entry in incoming {
        match entry.role {
            DialogRole::User => {
                // Scoped to the *tail*: only an unanswered prompt still sitting
                // at the end is a re-read. Comparing against the last user entry
                // anywhere in the dialog dropped genuine repeats — an approval
                // loop ("y", "retry", "all") re-sends the same text one turn
                // later — and, with that turn boundary now missing, the reply
                // that followed overwrote the *previous* turn's reply in the
                // Assistant arm below, silently losing two entries strictly
                // below the newest one.
                if dialog.last().is_some_and(|e| e.role == DialogRole::User && e.text == entry.text) {
                    continue;
                }
                if dialog.iter().any(|e| e.role == DialogRole::User && e.timestamp == entry.timestamp && e.text == entry.text) {
                    continue;
                }
                dialog.push(entry.clone());
                changed = true;
            }
            DialogRole::Assistant => {
                // Replayed delta carrying the identical entry — skip regardless
                // of turn structure. Mirrors the User arm, and is what keeps a
                // re-send idempotent once the turn sits behind a separator,
                // where the tail scan below deliberately stops.
                if dialog.iter().any(|e| e.role == DialogRole::Assistant && e.timestamp == entry.timestamp && e.text == entry.text) {
                    continue;
                }
                // A separator ends a turn exactly as a user entry does, so the
                // scan stops at both — otherwise a reply arriving first after a
                // `/clear` or compact boundary would reach back across it and
                // overwrite the pre-boundary reply.
                let tail_idx = dialog.iter().enumerate().rev()
                    .take_while(|(_, e)| e.role != DialogRole::User && e.role != DialogRole::Separator)
                    .find(|(_, e)| e.role == DialogRole::Assistant)
                    .map(|(i, _)| i);
                if let Some(i) = tail_idx {
                    if dialog[i].text == entry.text {
                        continue;
                    }
                    dialog[i] = entry.clone();
                } else {
                    dialog.push(entry.clone());
                }
                changed = true;
            }
            DialogRole::Separator => {
                if dialog.last().is_some_and(|e| e.role == DialogRole::Separator) {
                    continue;
                }
                if dialog.iter().any(|e| e.role == DialogRole::Separator && e.timestamp == entry.timestamp) {
                    continue;
                }
                dialog.push(entry.clone());
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_is_live_work_but_blocked_is_not() {
        for st in [Status::Working, Status::Waiting] {
            assert!(st.is_live_work(), "{st:?} is a turn in flight that sleeping would suspend");
        }
        // Blocked is parked on the user — nothing progresses while the Mac is
        // asleep, so it must hold neither the lid veto (and with it, thermal
        // safety sleep) nor the idle-sleep assertion.
        for st in [Status::Idle, Status::Blocked, Status::Done, Status::Error] {
            assert!(!st.is_live_work(), "{st:?} should not hold the Mac awake");
        }
    }

    fn set(id: &str, status: Status, label: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: Some(label.to_string()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        }
    }

    fn set_no_label(id: &str, status: Status) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: None,
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        }
    }

    fn get<'a>(state: &'a AppState, id: &str) -> AgentSession {
        state
            .snapshot()
            .into_iter()
            .find(|s| s.id == id)
            .expect("session")
    }

    const NO_CONTINUATIONS: &[String] = &[];

    #[test]
    fn restore_row_backdates_the_state_clock_while_stamping_updated_now() {
        // The two are different facts and sit adjacent in the signature, so this
        // is what catches them being swapped: age is read by the widget's elapsed
        // clock, `/api/agents`' `status_age_ms` and the notifier's time-in-state,
        // and `notifications::state_observed_here` reads it to decide a restored
        // state is not re-announced. A `state_entered_at` of `now` defeats all
        // four at once.
        let state = AppState::new();
        assert!(state.restore_row(set_no_label("dash", Status::Blocked), false, 1_000, 900_000, None));
        let s = get(&state, "dash");
        assert_eq!(s.state_entered_at, 1_000, "the status began long before we started");
        assert_eq!(s.updated, 900_000, "but the row was written just now");
        assert_eq!(s.status, Status::Blocked);
    }

    #[test]
    fn restore_row_leaves_an_existing_row_completely_alone() {
        // The race this method exists for: the axum server is already accepting
        // hook events when the reconcile runs, and `apply_set`'s existing-row
        // branch would overwrite `status` and reset `state_entered_at` — stamping
        // a genuinely Working session back to whatever its tab said.
        let state = AppState::new();
        state.apply_set(set("dash", Status::Working, "live prompt"), 500_000, NO_CONTINUATIONS, None);
        assert!(!state.restore_row(set_no_label("dash", Status::Blocked), false, 1_000, 900_000, None), "no row was created");
        let s = get(&state, "dash");
        assert_eq!(s.status, Status::Working, "the live status survives");
        assert_eq!(s.label, "live prompt");
        assert_eq!(s.state_entered_at, 500_000, "and its clock is untouched");
    }

    #[test]
    fn a_snapshot_taken_after_a_mutation_outranks_one_taken_before_it() {
        // The property `terminal_title::sync` decides with. Two emits are in
        // flight, each snapshotted at a different moment; the one holding the
        // newer rows must carry the higher ticket, or the older one writes its
        // glyph last and the tab contradicts the row until some later emit moves
        // it. What this cannot pin is the part that makes it true under
        // concurrency — that the ticket is minted inside the `sessions` lock —
        // since a test that races the two would pass on a broken implementation
        // most of the time.
        let state = AppState::new();
        let (before, rows_before) = state.snapshot_versioned();
        state.apply_set(set("dash", Status::Blocked, "has a question"), 1_000, NO_CONTINUATIONS, None);
        let (after, rows_after) = state.snapshot_versioned();
        assert!(after > before, "a snapshot of the newer rows must outrank one of the older: {after} vs {before}");
        assert!(rows_before.is_empty(), "the earlier snapshot predates the row");
        assert_eq!(rows_after[0].status, Status::Blocked, "and the later one carries it");
    }

    #[test]
    fn tickets_are_strictly_increasing_and_never_zero() {
        // Strictly increasing, because `sync` stands down on `<=`: two snapshots
        // taken between the same pair of mutations would otherwise tie, and the
        // second would be dropped rather than re-asserting the title — which is
        // what the `REASSERT_MS` re-push exists to do. Never zero, because that is
        // `commands::resolved_snapshot_versioned`'s "there was no `AppState` to
        // ask", and every consumer reads it as older than anything it has applied.
        let state = AppState::new();
        let first = state.snapshot_versioned().0;
        assert_eq!(first, 1);
        assert!(state.snapshot_versioned().0 > first);
    }

    #[test]
    fn restore_row_seeds_the_persisted_conversation() {
        let state = AppState::new();
        let persisted = PersistedSession {
            dialog: vec![DialogEntry { role: DialogRole::User, text: "do the thing".into(), timestamp: 10, status: Status::Working, task_start: true, boundary: None }],
            original_prompt: Some("do the thing".into()),
            delegated_task: None,
            message_line: None,
            task_started_at: 10,
        };
        assert!(state.restore_row(set_no_label("dash", Status::Done), false, 1_000, 900_000, Some(persisted)));
        let s = get(&state, "dash");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.original_prompt.as_deref(), Some("do the thing"));
        assert_eq!(s.task_started_at, 10);
        assert_eq!(s.label, "", "no label was ever observed, and inventing one is not this path's business");
    }

    #[test]
    fn restore_row_honors_the_separator_rule_exactly_as_the_hook_path_does() {
        // Shared through `new_session`, so a cleared conversation restores its
        // history without resurrecting the pre-boundary task.
        let state = AppState::new();
        let persisted = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 10, status: Status::Working, task_start: true, boundary: None },
                DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 20, status: Status::Idle, task_start: false, boundary: None },
            ],
            original_prompt: Some("old task".into()),
            delegated_task: None,
            message_line: None,
            task_started_at: 10,
        };
        assert!(state.restore_row(set_no_label("dash", Status::Idle), false, 1_000, 900_000, Some(persisted)));
        let s = get(&state, "dash");
        assert_eq!(s.dialog.len(), 2, "history kept");
        assert_eq!(s.original_prompt, None, "but no task is in flight");
        assert_eq!(s.task_started_at, 0);
    }

    // -------- attention (finished-and-unread) tests --------

    #[test]
    fn done_row_is_pending_until_marked_then_seen() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 10_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").attention(), Attention::Pending);

        assert!(state.mark_attended("a", 12_000), "verdict changed");
        assert_eq!(get(&state, "a").attention(), Attention::Seen);
    }

    #[test]
    fn a_new_finish_re_raises_attention_with_no_reset_call() {
        // The whole point of anchoring on content rather than storing a bool:
        // nothing anywhere clears `attended_at`, yet a fresh turn must come back
        // unread. `state_entered_at` moving forward is what does it.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task one"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 10_000, NO_CONTINUATIONS, None);
        state.mark_attended("a", 11_000);
        assert_eq!(get(&state, "a").attention(), Attention::Seen);

        state.apply_set(set("a", Status::Working, "task two"), 20_000, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 30_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").attention(), Attention::Pending, "second finish is unread again");
        assert!(get(&state, "a").attended_at.is_some(), "the old stamp is still there — it's simply stale");
    }

    #[test]
    fn assistant_text_flushed_after_stop_re_raises_attention() {
        // `Stop` settles the row Done *before* Claude Code flushes the final
        // reply, and `apply_text_entries` bumps only `updated`. Without the
        // dialog term in `content_at`, a read landing in that gap would count as
        // having seen text that hadn't arrived.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 10_000, NO_CONTINUATIONS, None);
        state.mark_attended("a", 10_500);
        assert_eq!(get(&state, "a").attention(), Attention::Seen);

        state.apply_text_entries("a", &[(DialogRole::Assistant, "here is the answer".into())], 11_000);
        assert_eq!(get(&state, "a").attention(), Attention::Pending, "the reply arrived after the read");
    }

    #[test]
    fn non_done_rows_are_moot_regardless_of_stamp() {
        let state = AppState::new();
        for status in [Status::Working, Status::Waiting, Status::Blocked, Status::Error, Status::Idle] {
            state.apply_set(set_no_label("a", status), 0, NO_CONTINUATIONS, None);
            state.mark_attended("a", 5_000);
            assert_eq!(get(&state, "a").attention(), Attention::Moot, "{status:?} never asks to be read");
        }
    }

    #[test]
    fn mark_attended_reports_only_real_verdict_changes() {
        // What keeps the presence sensor off the emit path: a re-stamp on an
        // already-read row must not fire an event every tick while the user types.
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 0, NO_CONTINUATIONS, None);
        assert!(state.mark_attended("a", 1_000), "Pending -> Seen is a change");
        assert!(!state.mark_attended("a", 2_000), "Seen -> Seen is not");

        state.apply_set(set_no_label("b", Status::Working), 0, NO_CONTINUATIONS, None);
        assert!(!state.mark_attended("b", 1_000), "a Moot row never changes verdict");
        assert!(!state.mark_attended("nope", 1_000), "an unknown id is not a change");
    }

    #[test]
    fn mark_attended_is_monotonic() {
        // A slow poll can answer with an observation older than one already held;
        // it must not walk the stamp backwards and un-read a row.
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 0, NO_CONTINUATIONS, None);
        state.mark_attended("a", 5_000);
        state.mark_attended("a", 1_000);
        assert_eq!(get(&state, "a").attended_at, Some(5_000));
    }

    #[test]
    fn mark_attended_does_not_bump_updated() {
        // `updated` is the compare-and-swap guard the liveness reaper and
        // `settle_stale_waiting` abort on, and the reaper's dead-streak counter
        // needs it to stay still. Looking at a row must not disturb either.
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 7_000, NO_CONTINUATIONS, None);
        state.mark_attended("a", 9_000);
        assert_eq!(get(&state, "a").updated, 7_000);
    }

    #[test]
    fn clear_all_attention_restores_pre_feature_rendering() {
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 0, NO_CONTINUATIONS, None);
        state.mark_attended("a", 1_000);
        assert!(state.clear_all_attention());
        assert_eq!(get(&state, "a").attention(), Attention::Pending);
        assert!(!state.clear_all_attention(), "idempotent once nothing is stamped");
    }

    #[test]
    fn the_observation_itself_never_reaches_the_wire() {
        // What crosses a wire is a *verdict*, on one route, and only to this
        // user's own machines: `sync::build_push` reads `attention()` and
        // advertises it as `SessionSync::attended`. The observation behind it
        // does not travel at all, and neither does the enum — so a reader of a
        // serialized row has no way to learn when a human sat at this keyboard,
        // only whether the row is still asking.
        //
        // `read` is asserted by value rather than by absence of a key: the sync
        // pusher serializes a raw `AppState` row, where a *local* row's flag
        // must still be `false` however long ago the user looked, because only
        // the display path stamps it. The per-route half is pinned next to the
        // code that does it, in `sync`'s
        // `the_senders_own_verdict_rides_the_push_and_the_raw_row_does_not`.
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 0, NO_CONTINUATIONS, None);
        state.mark_attended("a", 1_000);
        let json = serde_json::to_value(get(&state, "a")).expect("serialize");
        assert!(json.get("attended_at").is_none(), "the observation stays on this machine");
        assert!(json.get("attention").is_none(), "and the enum is never serialized");
        assert_eq!(json.get("read").and_then(|v| v.as_bool()), Some(false), "a local row's flag is never true off the display path");
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("done"), "the wire reports what the agent did");
    }

    /// A synced row as `sync::ingest` leaves one, parked on this device.
    /// `content_seen_at` is the *local* instant the content arrived, which
    /// `ingest` always sets — a fixture without it would make every
    /// `mark_remote_attended` call refuse and the tests below vacuous.
    fn seed_remote_at(state: &AppState, id: &str, state_entered_at: i64, newest_reply: Option<i64>, arrived: Option<i64>) {
        seed_remote(state, id, state_entered_at, newest_reply);
        let mut remote = state.remote.lock().unwrap();
        remote.get_mut("laptop").expect("device").sessions[0].content_seen_at = arrived;
    }

    fn seed_remote(state: &AppState, id: &str, state_entered_at: i64, newest_reply: Option<i64>) {
        let holder = AppState::new();
        holder.apply_set(set_no_label(id, Status::Done), state_entered_at, NO_CONTINUATIONS, None);
        let mut s = holder.snapshot().pop().expect("row");
        s.id = format!("laptop/{id}");
        s.origin = Some("laptop".into());
        if let Some(ts) = newest_reply {
            s.dialog.push(DialogEntry { role: DialogRole::Assistant, text: "a reply".into(), timestamp: ts, status: Status::Done, task_start: false, boundary: None });
        }
        state.remote.lock().unwrap().insert(
            "laptop".to_string(),
            crate::state::RemoteDevice { sessions: vec![s], last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed },
        );
    }

    #[test]
    fn a_synced_row_is_stamped_with_its_own_content_watermark() {
        // Never this machine's clock: both terms of a synced row's `content_at`
        // were minted by the origin, so a local instant would be the one
        // comparison here that needs the two machines to agree about the time.
        // The value recorded is one the origin itself produced.
        let state = AppState::new();
        seed_remote_at(&state, "proj", 1_000, Some(7_000), Some(50));
        assert_eq!(state.mark_remote_attended("laptop/proj", 60), Some(7_000), "the newest thing it holds, not the instant of the look");
        assert_eq!(state.remote_snapshot()[0].attention(), Attention::Seen);
        assert_eq!(state.mark_remote_attended("laptop/proj", 60), None, "already read, so nothing to report");
        assert_eq!(state.mark_remote_attended("laptop/gone", 60), None);
    }

    #[test]
    fn an_observation_older_than_the_content_does_not_credit_a_synced_row() {
        // The gate that stops the stamp being a tautology. Stamping `content_at`
        // satisfies `attention()` by construction, so without this ANY
        // observation would credit the row however stale — and stale ones are
        // routine: `terminals::agterm` mints an input instant as `now - idle`
        // from the desktop-wide clock, so one keystroke into a tab, then walking
        // away with the terminal in front, re-offers that frozen instant every
        // tick. Both instants here are this machine's clock; the value stamped
        // is still the origin's.
        let state = AppState::new();
        seed_remote_at(&state, "proj", 1_000, Some(7_000), Some(50_000));
        assert_eq!(state.mark_remote_attended("laptop/proj", 49_999), None, "the look predates the answer arriving here");
        assert_eq!(state.remote_snapshot()[0].attention(), Attention::Pending, "so the row is still asking");
        assert_eq!(state.mark_remote_attended("laptop/proj", 50_000), Some(7_000), "a look at the moment it arrived counts");
    }

    #[test]
    fn a_synced_row_whose_content_never_arrived_here_credits_nothing() {
        // `ingest` writes an arrival on every push, so no recorded arrival means
        // this device has not heard about the row rather than that its content
        // is old. Refusing leaves it showing, which the next push corrects.
        let state = AppState::new();
        seed_remote_at(&state, "proj", 1_000, Some(7_000), None);
        assert_eq!(state.mark_remote_attended("laptop/proj", i64::MAX), None);
    }

    #[test]
    fn a_reply_arriving_after_a_synced_read_re_raises_it() {
        // The self-falsifying half has to hold for a synced row too, and it is
        // what makes erring early free: this device's dialog copy lags the
        // origin's while a pull is outstanding, so the watermark recorded is at
        // most the origin's, and the rest of the turn un-reads the row when it
        // lands.
        let state = AppState::new();
        seed_remote_at(&state, "proj", 1_000, Some(7_000), Some(50));
        state.mark_remote_attended("laptop/proj", 60);
        {
            let mut remote = state.remote.lock().unwrap();
            let s = &mut remote.get_mut("laptop").expect("device").sessions[0];
            s.dialog.push(DialogEntry { role: DialogRole::Assistant, text: "the rest".into(), timestamp: 9_000, status: Status::Done, task_start: false, boundary: None });
        }
        assert_eq!(state.remote_snapshot()[0].attention(), Attention::Pending, "the pull moved the watermark past the stamp");
        // The later reply has to be recorded as *arriving* here before a look can
        // vouch for it — the same gate, now doing the work the dialog pull makes
        // it do in production.
        state.remote.lock().unwrap().get_mut("laptop").expect("device").sessions[0].content_seen_at = Some(70);
        assert_eq!(state.mark_remote_attended("laptop/proj", 80), Some(9_000), "and reading it again reports the whole of it");
    }

    #[test]
    fn the_clamp_ceiling_is_the_rows_own_watermark() {
        // What `sync::post_attention` bounds an incoming peer report to.
        // Unclamped, `mark_attended` only moves forward, so a value from the far
        // future would mark the row read for every turn it ever has.
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 1_000, NO_CONTINUATIONS, None);
        assert_eq!(state.content_at_of("a"), Some(1_000));
        assert_eq!(state.content_at_of("nobody"), None, "and a row we do not have has no ceiling to offer");
    }

    #[test]
    fn the_teardown_forgets_every_observation_and_no_verdict() {
        // A synced row carries one of each, and they go different ways. The
        // `attended_at` is an observation THIS machine made through a tab here,
        // so it is cleared with the local ones — the fixture sets it, because a
        // `None` there is exactly the state that would hide the bug. The `read`
        // is the origin's verdict, which this machine cannot erase: the next push
        // re-advertises it, so `commands::forget_read` declines to draw it
        // instead (pinned by `commands`'
        // `the_feature_switch_governs_a_synced_verdict_too`).
        let state = AppState::new();
        state.apply_set(set_no_label("a", Status::Done), 0, NO_CONTINUATIONS, None);
        state.mark_attended("a", 1_000);
        let mut remote = get(&state, "a");
        remote.id = "laptop/b".into();
        remote.origin = Some("laptop".into());
        remote.attended_at = Some(2_000);
        remote.read = true;
        state.remote.lock().unwrap().insert(
            "laptop".to_string(),
            crate::state::RemoteDevice { sessions: vec![remote], last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed },
        );

        assert!(state.clear_all_attention());
        assert_eq!(get(&state, "a").attention(), Attention::Pending, "the local observation is gone");
        assert_eq!(state.remote_snapshot()[0].attended_at, None, "and so is the one made here about a synced row");
        assert!(state.remote_snapshot()[0].read, "the origin's verdict is not this machine's to erase");
        assert!(!state.clear_all_attention(), "idempotent once nothing is stamped");
    }

    #[test]
    fn nothing_evidence_free_can_produce_a_clean_row() {
        // The invariant the whole redefinition rests on. `Idle` asserts there is
        // nothing to come back to; every path below knows strictly less than
        // that, so each must land on the neutral sink instead. This is written
        // as a sweep rather than a grep because the ways in are functions, not
        // literals — five of them used to reach for `Idle` and every one of them
        // would have shipped a row claiming to be wrapped up.
        assert_eq!(Status::default(), Status::Done, "a value nobody chose is not a claim");

        let state = AppState::new();

        // The path the first cut of this got wrong, and the commonest one: a
        // row created CLEAN by `/clear`, typed into, then Esc-cancelled. The
        // prior resting state really was `Idle`, and restoring it verbatim
        // would let a cancel write a clean claim over a turn that may have
        // edited files.
        state.apply_set(set_no_label("clean", Status::Idle), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("clean", Status::Working, "edit the thing"), 2_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "clean").status_before_working, Status::Idle, "the capture is faithful");
        assert_eq!(state.revert_cancelled_turn("clean", 3_000).expect("reverted").0, Status::Done, "but the revert refuses to restore CLEAN");

        // A row invented because a subagent asked for permission.
        let o = state.open_subagent_prompt(set("a", Status::Blocked, "needs approval: Bash"), prompt_from("agent-1", "Bash"), 1_000, None);
        assert_ne!(o.base_status, Status::Idle);

        // A first turn cancelled with Esc, which has no prior state to return to.
        state.apply_set(set("b", Status::Working, "do a thing"), 1_000, NO_CONTINUATIONS, None);
        assert_ne!(state.revert_cancelled_turn("b", 2_000).expect("reverted").0, Status::Idle);

        // A WAIT the backstop gave up on.
        state.apply_set(set("c", Status::Waiting, "bg work"), 1_000, NO_CONTINUATIONS, None);
        let updated = get(&state, "c").updated;
        assert!(state.settle_stale_waiting("c", updated, 9_000));
        assert_ne!(get(&state, "c").status, Status::Idle);

        // A row deserialized off the sync wire, where `status_before_working` is
        // filled by `Default` and reaches `revert_cancelled_turn` on the peer.
        let wire = serde_json::to_string(&get(&state, "c")).expect("serialize");
        let back: AgentSession = serde_json::from_str(&wire).expect("deserialize");
        assert_ne!(back.status_before_working, Status::Idle);
    }

    #[test]
    fn only_a_clear_boundary_licenses_a_clean_resume() {
        // Every removal appends a separator, so "ends with a separator" is true
        // after a compaction and after every ordinary exit too — and sessions
        // here start with `--continue`. Reading the weaker test would declare
        // nearly every row on the machine clean at start-up.
        let sep = |kind| vec![DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 10, status: Status::Done, task_start: false, boundary: kind }];
        assert!(ends_with_clear_boundary(&sep(Some(BoundaryKind::Clear))));
        assert!(!ends_with_clear_boundary(&sep(Some(BoundaryKind::Compact))));
        assert!(!ends_with_clear_boundary(&sep(Some(BoundaryKind::Ended))));
        assert!(!ends_with_clear_boundary(&sep(None)), "an untagged separator from an older build is not evidence");
        assert!(!ends_with_clear_boundary(&[]));
        assert!(!ends_with_clear_boundary(&[user_entry("still talking", 20)]));
    }

    #[test]
    fn revert_cancelled_turn_banks_elapsed_and_falls_back_to_done() {
        // A fresh session's first turn has no prior status, so a cancel reverts
        // to `Done` — the neutral sink `status_before_working` defaults to. Not
        // `Idle`: the cancelled turn may well have edited files, and a row that
        // has done work is the last thing that should read as wrapped up.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);

        assert_eq!(state.revert_cancelled_turn("a", 20_000), Some((Status::Done, false)));
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Done);
        assert_eq!(s.working_accumulated_ms, 20_000, "elapsed run banked");
        assert_eq!(s.state_entered_at, 20_000);
        assert_eq!(s.updated, 20_000);
    }

    #[test]
    fn revert_cancelled_turn_restores_blocked_after_aborted_reply() {
        // The reported bug: agent asks a question (Blocked), the user submits a
        // reply (Working) then cancels it with Esc. The row must revert to
        // Blocked — not Idle — so the user's *real* answer is an approval-cycle
        // reply (Blocked → Working, no boundary) and original_prompt survives.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix the parser"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "Push?"), 10_000, NO_CONTINUATIONS, None);
        // User submits a typo'd reply, which enters Working from Blocked...
        state.apply_set(set("a", Status::Working, "ny"), 12_000, NO_CONTINUATIONS, None);
        // ...then cancels it with Esc (no Stop hook) — the watcher calls
        // revert_cancelled_turn on the interrupt marker.
        assert_eq!(state.revert_cancelled_turn("a", 13_000), Some((Status::Blocked, false)));
        let reverted = get(&state, "a");
        assert_eq!(reverted.status, Status::Blocked, "cancelled reply reverts to the pending question");

        // The real answer now lands from Blocked — an approval cycle, not a task
        // boundary — so the task is preserved even without a continuation match.
        state.apply_set(set("a", Status::Working, "y"), 14_000, NO_CONTINUATIONS, None);
        let answered = get(&state, "a");
        assert_eq!(answered.original_prompt.as_deref(), Some("fix the parser"), "answer must not clobber the task");
    }

    #[test]
    fn revert_cancelled_turn_leaves_a_settled_turn_alone() {
        for settled in [Status::Done, Status::Idle, Status::Waiting, Status::Error] {
            let state = AppState::new();
            state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
            state.apply_set(set("a", settled, "fix foo.py"), 10_000, NO_CONTINUATIONS, None);
            assert_eq!(state.revert_cancelled_turn("a", 20_000), None, "{settled:?}");
            let s = get(&state, "a");
            assert_eq!((s.status, s.state_entered_at), (settled, 10_000), "{settled:?}");
        }
    }

    #[test]
    fn revert_cancelled_turn_reverts_a_turn_cancelled_on_a_dialog() {
        // Esc on an AskUserQuestion (or a permission dialog) writes the interrupt
        // marker with no hook, while PreToolUse has the row on BLOCK. The dialog
        // is gone, so the row goes back to where the prompt found it.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "second task"), 2_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "has a question"), 5_000, NO_CONTINUATIONS, None);

        assert_eq!(state.revert_cancelled_turn("a", 8_000), Some((Status::Done, false)));
        let s = get(&state, "a");
        assert_eq!((s.status, s.state_entered_at, s.updated), (Status::Done, 8_000, 8_000));
        assert_eq!(s.working_accumulated_ms, 3_000, "the run was banked on entering Blocked; the dialog's wait is not work");
    }

    #[test]
    fn settle_stale_waiting_moves_waiting_to_done() {
        // A row wedged in Waiting (killed background task, no clearing signal) is
        // settled to Done, with the label preserved and the timestamps advanced.
        let state = AppState::new();
        state.apply_set(set("a", Status::Waiting, "run tests"), 0, NO_CONTINUATIONS, None);
        assert!(state.settle_stale_waiting("a", 0, 600_000));
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Done);
        assert_eq!(s.label, "run tests", "the task label carries over into Done");
        assert_eq!(s.state_entered_at, 600_000);
        assert_eq!(s.updated, 600_000);
    }

    #[test]
    fn settle_stale_waiting_is_guarded_by_updated() {
        // A follow-up turn / new prompt bumped `updated` after the tick's
        // snapshot — the stale-`updated` guard must abort so a row that just
        // moved on isn't clobbered back to Done.
        let state = AppState::new();
        state.apply_set(set("a", Status::Waiting, "run tests"), 0, NO_CONTINUATIONS, None);
        assert!(!state.settle_stale_waiting("a", 999, 600_000), "mismatched updated aborts");
        assert_eq!(get(&state, "a").status, Status::Waiting);
    }

    #[test]
    fn settle_stale_waiting_is_noop_when_not_waiting() {
        // Only Waiting is time-settled; a Done/Working/etc. row is left alone.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "do a thing"), 0, NO_CONTINUATIONS, None);
        assert!(!state.settle_stale_waiting("a", 0, 600_000));
        assert_eq!(get(&state, "a").status, Status::Working);
    }

    #[test]
    fn set_drift_flags_and_clears_orthogonally_to_status() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Done, "done"), 0, NO_CONTINUATIONS, None);
        // Flag it — changed, `updated` bumped, status untouched.
        assert!(state.set_drift("a", true, 1_000));
        let s = get(&state, "a");
        assert!(s.instruction_drift);
        assert_eq!(s.status, Status::Done, "drift rides alongside status, never changes it");
        assert_eq!(s.updated, 1_000);
        // Same value again is a no-op (and doesn't bump `updated`).
        assert!(!state.set_drift("a", true, 2_000));
        assert_eq!(get(&state, "a").updated, 1_000);
        // Clearing flips it back.
        assert!(state.set_drift("a", false, 3_000));
        assert!(!get(&state, "a").instruction_drift);
        // Unknown row is a no-op.
        assert!(!state.set_drift("nope", true, 4_000));
    }

    #[test]
    fn drift_confirmed_reads_the_row_or_false() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Blocked, "q"), 0, NO_CONTINUATIONS, None);
        assert!(!state.drift_confirmed("a"));
        state.set_drift("a", true, 1_000);
        assert!(state.drift_confirmed("a"));
        // Missing row reads as not-drifted.
        assert!(!state.drift_confirmed("nope"));
    }

    #[test]
    fn clear_all_drift_clears_every_flagged_row_and_is_idempotent() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Done, "x"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("b", Status::Working, "y"), 0, NO_CONTINUATIONS, None);
        state.set_drift("a", true, 100);
        // Only "a" is flagged → clearing changes it; status preserved, "b" untouched.
        assert!(state.clear_all_drift(200));
        assert!(!get(&state, "a").instruction_drift);
        assert_eq!(get(&state, "a").status, Status::Done, "clearing drift never changes status");
        assert_eq!(get(&state, "a").updated, 200);
        // Nothing flagged now → no-op, `updated` untouched.
        assert!(!state.clear_all_drift(300));
        assert_eq!(get(&state, "a").updated, 200);
    }

    #[test]
    fn new_working_session_captures_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 1000, NO_CONTINUATIONS, None);

        let s = get(&state, "a");
        assert_eq!(s.status, Status::Working);
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(s.state_entered_at, 1000);
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn new_non_working_session_has_no_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Idle, ""), 1000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").original_prompt, None);
    }

    #[test]
    fn approval_cycle_preserves_original_prompt_and_accumulator() {
        let state = AppState::new();
        // Initial working: task starts
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        // Claude asks for approval after 30s
        state.apply_set(set("a", Status::Blocked, "run bash?"), 30_000, NO_CONTINUATIONS, None);
        let mid = get(&state, "a");
        assert_eq!(mid.status, Status::Blocked);
        assert_eq!(mid.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(mid.working_accumulated_ms, 30_000);
        assert_eq!(mid.state_entered_at, 30_000);

        // User approves after 5s; agent resumes working with noise label "yes"
        state.apply_set(set("a", Status::Working, "yes"), 35_000, NO_CONTINUATIONS, None);
        let resumed = get(&state, "a");
        assert_eq!(resumed.status, Status::Working);
        assert_eq!(
            resumed.original_prompt.as_deref(),
            Some("fix foo.py"),
            "original prompt must survive approval cycle"
        );
        assert_eq!(
            resumed.working_accumulated_ms, 30_000,
            "accumulated time from before the approval must be preserved"
        );
        assert_eq!(resumed.state_entered_at, 35_000);
    }

    #[test]
    fn done_then_working_is_task_boundary_and_resets_state() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "fixed!"), 60_000, NO_CONTINUATIONS, None);
        let after_done = get(&state, "a");
        assert_eq!(
            after_done.working_accumulated_ms, 60_000,
            "working time accumulated on exit"
        );
        assert_eq!(after_done.original_prompt.as_deref(), Some("fix foo.py"));

        // New task on the same session
        state.apply_set(set("a", Status::Working, "add tests"), 120_000, NO_CONTINUATIONS, None);
        let new_task = get(&state, "a");
        assert_eq!(new_task.original_prompt.as_deref(), Some("add tests"));
        assert_eq!(new_task.working_accumulated_ms, 0);
        assert_eq!(new_task.state_entered_at, 120_000);
    }

    #[test]
    fn idle_then_working_is_also_task_boundary() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Idle, ""), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "new task"), 10_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("new task"));
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn working_to_error_accumulates_but_does_not_reset() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "do a thing"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Error, "perm denied"), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Error);
        assert_eq!(s.working_accumulated_ms, 5_000);
        assert_eq!(s.original_prompt.as_deref(), Some("do a thing"));
        assert_eq!(s.label, "perm denied");
    }

    #[test]
    fn same_non_working_status_update_keeps_state_entered_at() {
        // For non-Working same-status updates (e.g. successive Blocked events
        // refining the question), state_entered_at must not bounce. Working →
        // Working is now a task boundary on purpose — see the cancellation tests.
        let state = AppState::new();
        state.apply_set(set("a", Status::Blocked, "ask"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "ask"), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.state_entered_at, 0, "state_entered_at must not reset within the same non-Working status");
    }

    #[test]
    fn working_to_working_with_new_prompt_is_task_boundary() {
        // Cancellation case: user hits Esc mid-task and submits a new prompt
        // without an intervening Stop, so the row never leaves Working. The
        // new prompt must be treated as a fresh task: original_prompt
        // re-captured, working timer reset.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "second task"), 30_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("second task"));
        assert_eq!(s.working_accumulated_ms, 0, "task boundary zeroes the accumulator");
        assert_eq!(s.state_entered_at, 30_000, "task boundary resets segment start even when status is unchanged");
    }

    #[test]
    fn working_to_working_continuation_prompt_does_not_reset() {
        // Even when the prior status is Working, a continuation prompt must
        // suppress the boundary so original_prompt and the timer are preserved.
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo"), 0, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 5_000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.state_entered_at, 0, "continuation suppresses segment-start reset");
        assert_eq!(s.working_accumulated_ms, 0);
    }

    #[test]
    fn take_session_removes_and_returns_the_row() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("b", Status::Working, "other"), 0, NO_CONTINUATIONS, None);
        let removed = state.take_session("a", None, BoundaryKind::Clear, 0);
        assert!(removed.is_some(), "the removed session is returned");
        assert_eq!(removed.unwrap().id, "a");
        let ids: Vec<String> = state.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn take_session_aborts_when_updated_moved() {
        // The reaper passes the last-seen `updated`; if an event bumped it
        // between observation and removal, take_session must not delete the row.
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        let updated = get(&state, "a").updated;
        assert!(state.take_session("a", Some(updated + 1), BoundaryKind::Clear, 0).is_none(), "stale expectation aborts");
        assert_eq!(state.snapshot().len(), 1, "row survives a mismatched expectation");
        assert!(state.take_session("a", Some(updated), BoundaryKind::Clear, 0).is_some(), "matching expectation removes");
        assert!(state.snapshot().is_empty());
    }

    #[test]
    fn take_session_appends_boundary_before_removing() {
        // A removed dialog should end with a separator so a restored copy starts
        // a clean task (same continuity /clear relies on).
        let state = AppState::new();
        let mut input = set("a", Status::Working, "task");
        input.dialog_entry = Some(PendingDialogEntry { role: DialogRole::User, text: "task".into() });
        state.apply_set(input, 0, NO_CONTINUATIONS, None);
        let removed = state.take_session("a", None, BoundaryKind::Clear, 100).expect("removed");
        assert_eq!(removed.dialog.last().map(|e| e.role), Some(DialogRole::Separator));
    }

    #[test]
    fn model_and_tokens_are_updated_when_provided() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(
            SetInput {
                id: "a".into(),
                status: Status::Working,
                label: Some("task".into()),
                source: None,
                model: Some("claude-opus-4-7".into()),
                input_tokens: Some(50_000),
                dialog_entry: None,
                waiting_backstop_armed: false,
                turn_from_relay: None,
                delegated_task: None,
                message_line: None,
                message_is_reply: None,
            },
            1000,
            NO_CONTINUATIONS,
            None,
        );
        let s = get(&state, "a");
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.input_tokens, Some(50_000));
    }

    #[test]
    fn missing_label_preserves_prior_label() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Blocked), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.label, "fix foo.py", "label must survive a set with no label field");
        assert_eq!(s.status, Status::Blocked);
    }

    #[test]
    fn task_boundary_with_missing_label_preserves_prior_original_prompt() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 10_000, NO_CONTINUATIONS, None);
        // New task starts, but hook didn't send a prompt label (e.g. prompt
        // wasn't captured) — original_prompt should remain whatever it was.
        state.apply_set(set_no_label("a", Status::Working), 20_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
        assert_eq!(s.working_accumulated_ms, 0, "still resets accumulator on task boundary");
    }

    #[test]
    fn continuation_prompt_after_done_does_not_reset_task() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into(), "continue".into(), "proceed".into()];
        // Original task
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        // Agent finishes
        state.apply_set(set_no_label("a", Status::Done), 60_000, &cont, None);
        let after_done = get(&state, "a");
        assert_eq!(after_done.working_accumulated_ms, 60_000);
        assert_eq!(after_done.original_prompt.as_deref(), Some("fix foo.py"));
        // User types "go" — should be treated as a continuation, not a new task
        state.apply_set(set("a", Status::Working, "go"), 80_000, &cont, None);
        let resumed = get(&state, "a");
        assert_eq!(
            resumed.original_prompt.as_deref(),
            Some("fix foo.py"),
            "continuation prompt must NOT re-capture original_prompt"
        );
        assert_eq!(
            resumed.working_accumulated_ms, 60_000,
            "continuation prompt must NOT reset the working timer"
        );
        assert_eq!(resumed.label, "go");
    }

    #[test]
    fn default_affirmations_do_not_clobber_task_after_done() {
        // End-to-end guard against the recurring "row shows 'y' as the task"
        // bug: an approval reply can arrive when the row is Done or Idle rather
        // than Blocked — e.g. the user cancelled a mis-typed reply with Esc
        // (the watcher reverts the turn via the interrupt marker), then typed the
        // real "y". From
        // Done/Idle that "y" would be a task boundary; with the default
        // continuation list it must preserve original_prompt instead.
        let cont = crate::config::Config::default().continuation_prompts;
        for reply in ["y", "yes", "yeah", "yep", "yup", "ok", "okay", "sure", "Yes", " y "] {
            let state = AppState::new();
            state.apply_set(set("a", Status::Working, "fix the parser"), 0, &cont, None);
            state.apply_set(set_no_label("a", Status::Done), 10_000, &cont, None);
            state.apply_set(set("a", Status::Working, reply), 20_000, &cont, None);
            let s = get(&state, "a");
            assert_eq!(s.original_prompt.as_deref(), Some("fix the parser"), "reply {reply:?} clobbered the task");
            assert_eq!(s.working_accumulated_ms, 10_000, "reply {reply:?} reset the timer");
        }
    }

    #[test]
    fn continuation_match_is_case_insensitive_and_trimmed() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into(), "Continue".into()];
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 1000, &cont, None);
        // Match against "Go" (uppercase) and surrounding whitespace
        state.apply_set(set("a", Status::Working, "  Go  "), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
    }

    #[test]
    fn non_continuation_prompt_after_done_still_resets() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 1000, &cont, None);
        // "go ahead" is NOT in the list — exact match only
        state.apply_set(set("a", Status::Working, "go ahead"), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(
            s.original_prompt.as_deref(),
            Some("go ahead"),
            "non-exact-match prompt should re-capture as a fresh task"
        );
    }

    #[test]
    fn task_boundary_updates_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 30_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "second task"), 60_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("second task"));
        assert_eq!(s.task_started_at, 60_000);
    }

    #[test]
    fn approval_cycle_preserves_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "permission?"), 5_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "yes"), 6_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.task_started_at, 0, "task_started_at survives the approval cycle");
    }

    #[test]
    fn continuation_prompt_preserves_task_started_at() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        state.apply_set(set("a", Status::Working, "fix foo"), 0, &cont, None);
        state.apply_set(set_no_label("a", Status::Done), 10_000, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 20_000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo"));
        assert_eq!(s.task_started_at, 0, "continuation preserves task_started_at");
    }

    #[test]
    fn first_working_prompt_sets_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.task_started_at, 1_000);
    }

    #[test]
    fn boundary_with_missing_label_preserves_task_started_at() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first"), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Done, "done"), 5_000, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Working), 10_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("first"));
        assert_eq!(s.task_started_at, 1_000, "preserved when prompt is preserved");
    }

    // ----- dialog entry creation -----

    fn set_with_dialog(id: &str, status: Status, label: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: Some(label.to_string()),
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: Some(PendingDialogEntry {
                role: DialogRole::User,
                text: label.to_string(),
            }),
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        }
    }

    fn stop_with_dialog(id: &str, status: Status, agent_text: &str) -> SetInput {
        SetInput {
            id: id.to_string(),
            status,
            label: None,
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: Some(PendingDialogEntry {
                role: DialogRole::Assistant,
                text: agent_text.to_string(),
            }),
            waiting_backstop_armed: false,
            turn_from_relay: None,
            delegated_task: None,
            message_line: None,
            message_is_reply: None,
        }
    }

    #[test]
    fn dialog_entry_pushed_for_user_prompt() {
        let state = AppState::new();
        state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::User);
        assert_eq!(s.dialog[0].text, "fix foo");
        assert_eq!(s.dialog[0].timestamp, 1_000);
        assert_eq!(s.dialog[0].status, Status::Working);
    }

    #[test]
    fn dialog_entry_pushed_for_assistant_stop() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        state.apply_set(stop_with_dialog("a", Status::Done, "All fixed."), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::Assistant);
        assert_eq!(s.dialog[0].text, "All fixed.");
        assert_eq!(s.dialog[0].status, Status::Done);
    }

    #[test]
    fn dialog_entry_task_start_marks_only_boundaries() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        // First prompt creates the session — a task start.
        state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 0, &cont, None);
        // Agent finishes.
        state.apply_set(stop_with_dialog("a", Status::Done, "done"), 1_000, &cont, None);
        // Continuation "go" after done — resumes the task, not a new one.
        state.apply_set(set_with_dialog("a", Status::Working, "go"), 2_000, &cont, None);
        // A genuinely new top-level prompt — a task start again.
        state.apply_set(set_with_dialog("a", Status::Working, "next task"), 3_000, &cont, None);
        let s = get(&state, "a");
        let users: Vec<&DialogEntry> = s.dialog.iter().filter(|e| e.role == DialogRole::User).collect();
        assert_eq!(users[0].text, "fix foo");
        assert!(users[0].task_start, "first prompt is a task start");
        assert_eq!(users[1].text, "go");
        assert!(!users[1].task_start, "continuation is not a task start");
        assert_eq!(users[2].text, "next task");
        assert!(users[2].task_start, "new prompt after working is a task start");
        let assistant = s.dialog.iter().find(|e| e.role == DialogRole::Assistant).unwrap();
        assert!(!assistant.task_start, "assistant entries are never task starts");
    }

    /// What the `UserPromptSubmit` adapter makes of `prompt`, as the HTTP layer
    /// hands it to `apply_set`.
    fn prompt_submitted(prompt: &str) -> SetInput {
        let cfg = crate::config::Config::default();
        match crate::adapters::claude::dispatch("UserPromptSubmit", &serde_json::json!({ "cwd": "d:/projects/a", "prompt": prompt }), &cfg) {
            crate::adapters::AdapterOutput::Set { input, .. } => input,
            other => panic!("expected Set, got {other:?}"),
        }
    }

    const HAND_BACK: &str = "<agent-message from=\"ab070651cd45c459e\">
[Subagent hand-back] The text below is the final report of a subagent this session delegated to.
  placeholder report
</agent-message>";
    const IDLE_NOTICE: &str = "[Cross-session idle notice] \"peer-2f\", which you asked to be notified about, is idle now — it finished a turn at 22:15.";

    /// A subagent's hand-back and a cross-session notice run a turn of the task
    /// already there: the row goes `Working`, keeps its task, label and timer,
    /// and the history records the prompt without marking a task start.
    #[test]
    fn a_harness_prompt_keeps_the_row_s_task() {
        for prompt in [HAND_BACK, IDLE_NOTICE] {
            let state = AppState::new();
            let mut first = prompt_submitted("fix the parser");
            first.delegated_task = Some("the sender's task".into());
            assert_eq!(first.id, "a");
            state.apply_set(first, 0, NO_CONTINUATIONS, None);
            state.apply_set(stop_with_dialog("a", Status::Done, "fixed"), 5_000, NO_CONTINUATIONS, None);
            let before = get(&state, "a");
            state.apply_set(prompt_submitted(prompt), 9_000, NO_CONTINUATIONS, None);
            let s = get(&state, "a");
            assert_eq!(s.status, Status::Working);
            assert_eq!((s.original_prompt.as_deref(), s.delegated_task.as_deref(), s.label.as_str()), (Some("fix the parser"), Some("the sender's task"), "fix the parser"));
            assert_eq!((s.task_started_at, s.working_accumulated_ms), (before.task_started_at, before.working_accumulated_ms), "the task's clock runs on");
            let last = s.dialog.last().expect("recorded");
            assert_eq!((last.text.as_str(), last.task_start), (prompt, false));
            assert_eq!(s.task_lines().iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["fix the parser"]);
        }
    }

    /// A `<task-notification>` wakes the agent when background work finishes; it
    /// becomes no dialog entry, and like a hand-back it is a turn of the task
    /// already there, so the working timer keeps what the task banked.
    #[test]
    fn a_task_notification_keeps_the_task_timer() {
        let state = AppState::new();
        state.apply_set(prompt_submitted("start the dev server"), 0, NO_CONTINUATIONS, None);
        state.apply_set(stop_with_dialog("a", Status::Waiting, "started"), 5_000, NO_CONTINUATIONS, None);
        let before = get(&state, "a");
        assert_eq!(before.working_accumulated_ms, 5_000);
        let notification = prompt_submitted("<task-notification>\n<status>completed</status>\n</task-notification>");
        assert!(notification.dialog_entry.is_none());
        state.apply_set(notification, 9_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Working);
        assert_eq!((s.original_prompt.as_deref(), s.task_started_at, s.working_accumulated_ms), (Some("start the dev server"), before.task_started_at, 5_000));
    }

    /// A person who pasted a report to ask about it is a person, and starts a
    /// task.
    #[test]
    fn a_person_asking_about_a_pasted_hand_back_starts_a_task() {
        let state = AppState::new();
        state.apply_set(prompt_submitted("fix the parser"), 0, NO_CONTINUATIONS, None);
        state.apply_set(stop_with_dialog("a", Status::Done, "fixed"), 5_000, NO_CONTINUATIONS, None);
        let asked = format!("{HAND_BACK}
why did this become the task?");
        state.apply_set(prompt_submitted(&asked), 9_000, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt, Some(crate::adapters::claude::clean_prompt(&asked)));
        assert!(s.dialog.last().is_some_and(|e| e.task_start));
    }

    /// A row whose first prompt is a harness prompt has no task, and nothing on
    /// the row shows the envelope in place of one.
    #[test]
    fn a_first_ever_harness_prompt_leaves_the_row_with_no_task() {
        for prompt in [HAND_BACK, IDLE_NOTICE] {
            let state = AppState::new();
            state.apply_set(prompt_submitted(prompt), 1_000, NO_CONTINUATIONS, None);
            let s = get(&state, "a");
            assert_eq!((s.status, s.original_prompt.as_deref(), s.label.as_str()), (Status::Working, None, ""));
            assert_eq!(s.dialog.len(), 1, "the history still records it");
            assert!(!s.dialog[0].task_start);
            assert_eq!((s.primary_text().as_ref(), s.row_line(), s.task_lines().len()), ("", None, 0));
        }
    }

    #[test]
    fn dialog_not_pushed_without_pending_entry() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        let s = get(&state, "a");
        assert!(s.dialog.is_empty());
    }

    #[test]
    fn dialog_restored_on_new_session() {
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true, boundary: None },
                DialogEntry { role: DialogRole::Assistant, text: "Done.".into(), timestamp: 200, status: Status::Done, task_start: false, boundary: None },
            ],
            original_prompt: Some("old task".into()),
            delegated_task: None,
            message_line: None,
            task_started_at: 100,
        };
        state.apply_set(set("a", Status::Done, "done"), 1_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
        assert_eq!(s.original_prompt.as_deref(), Some("old task"));
        assert_eq!(s.task_started_at, 100);
    }

    #[test]
    fn cleared_session_restore_keeps_dialog_but_drops_task() {
        // After `/clear`: SessionEnd marks a boundary separator + persists, then
        // SessionStart recreates the row from prompt_history with an Idle, no-label
        // Set. The restored dialog ends with the separator, so the row must come
        // back clean — no resurrected original_prompt — while keeping the history.
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true, boundary: None },
                DialogEntry { role: DialogRole::Assistant, text: "Done.".into(), timestamp: 200, status: Status::Done, task_start: false, boundary: None },
                DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 300, status: Status::Idle, task_start: false, boundary: None },
            ],
            original_prompt: Some("old task".into()),
            delegated_task: None,
            message_line: None,
            task_started_at: 100,
        };
        state.apply_set(set_no_label("a", Status::Idle), 1_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.status, Status::Idle);
        assert_eq!(s.original_prompt, None, "cleared row must not show the previous task");
        assert_eq!(s.task_started_at, 0, "cleared row starts with no task timer");
        assert_eq!(s.dialog.len(), 3, "dialog history is preserved for the history window");
    }

    #[test]
    fn cleared_session_restore_still_honors_incoming_prompt() {
        // If a Working prompt arrives on the same event that recreates a cleared
        // session, that's a genuine new task — it must win over the cleared state.
        let state = AppState::new();
        let restored = PersistedSession {
            dialog: vec![
                DialogEntry { role: DialogRole::User, text: "old task".into(), timestamp: 100, status: Status::Working, task_start: true, boundary: None },
                DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 300, status: Status::Idle, task_start: false, boundary: None },
            ],
            original_prompt: Some("old task".into()),
            delegated_task: None,
            message_line: None,
            task_started_at: 100,
        };
        state.apply_set(set("a", Status::Working, "new task"), 2_000, NO_CONTINUATIONS, Some(restored));
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("new task"));
        assert_eq!(s.task_started_at, 2_000);
    }

    #[test]
    fn apply_set_returns_true_when_dialog_changes() {
        let state = AppState::new();
        let changed = state.apply_set(set_with_dialog("a", Status::Working, "fix foo"), 0, NO_CONTINUATIONS, None);
        assert!(changed);
        let not_changed = state.apply_set(set("a", Status::Blocked, "question?"), 1_000, NO_CONTINUATIONS, None);
        assert!(!not_changed);
    }

    fn user_entry(text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::User, text: text.into(), timestamp: ts, status: Status::Working, task_start: true, boundary: None }
    }
    fn assistant_entry(text: &str, ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::Assistant, text: text.into(), timestamp: ts, status: Status::Done, task_start: false, boundary: None }
    }
    fn separator_entry(ts: i64) -> DialogEntry {
        DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: ts, status: Status::Idle, task_start: false, boundary: None }
    }

    fn seed(state: &AppState, dialog: Vec<DialogEntry>) {
        state.sessions.lock().unwrap().push(AgentSession {
            id: "a".into(),
            status: Status::Done,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog,
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: None,
            instruction_drift: false,
            terminal_stale_at: None,
            canary: Canary::Off,
            attended_at: None,
            content_seen_at: None,
            origin_label: None,
            turn_from_relay: false,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            subagent_gate: None,
        });
    }

    #[test]
    fn apply_text_appends_assistant_when_empty() {
        let state = AppState::new();
        seed(&state, vec![]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "first".into())], 100);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 1);
        assert_eq!(s.dialog[0].role, DialogRole::Assistant);
        assert_eq!(s.dialog[0].text, "first");
    }

    #[test]
    fn apply_text_appends_after_user() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "answer".into())], 20);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
        assert_eq!(s.dialog[1].text, "answer");
    }

    #[test]
    fn apply_text_replaces_assistant_in_same_turn() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10), assistant_entry("partial", 20)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "full".into())], 30);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2, "replaced in place, not appended");
        assert_eq!(s.dialog[1].text, "full");
    }

    #[test]
    fn apply_text_no_op_when_unchanged() {
        let state = AppState::new();
        seed(&state, vec![user_entry("hi", 10), assistant_entry("same", 20)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::Assistant, "same".into())], 30);
        assert!(!changed);
    }

    #[test]
    fn apply_text_interrupt_appends_after_user_boundary() {
        let state = AppState::new();
        seed(&state, vec![user_entry("task", 10)]);
        let entries = vec![
            (DialogRole::User, "interrupt".into()),
            (DialogRole::Assistant, "ack + pivot".into()),
            (DialogRole::Assistant, "final answer".into()),
        ];
        let changed = state.apply_text_entries("a", &entries, 50);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 3);
        assert_eq!(s.dialog[1].role, DialogRole::User);
        assert_eq!(s.dialog[1].text, "interrupt");
        assert_eq!(s.dialog[2].text, "final answer", "same-turn assistant texts replace");
    }

    #[test]
    fn apply_text_dedup_user_from_hook() {
        let state = AppState::new();
        seed(&state, vec![user_entry("fix bug", 10)]);
        let changed = state.apply_text_entries("a", &[(DialogRole::User, "fix bug".into())], 20);
        assert!(!changed, "hook already captured this prompt");
    }

    #[test]
    fn apply_text_missing_session_is_noop() {
        let state = AppState::new();
        assert!(!state.apply_text_entries("nope", &[(DialogRole::Assistant, "x".into())], 0));
    }

    #[test]
    fn mark_session_boundary_appends_separator() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), assistant_entry("a1", 20)]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 3);
        assert_eq!(s.dialog[2].role, DialogRole::Separator);
        assert_eq!(s.dialog[2].timestamp, 100);
        assert_eq!(s.updated, 100);
    }

    #[test]
    fn mark_session_boundary_noop_on_empty_dialog() {
        let state = AppState::new();
        seed(&state, vec![]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(!changed);
        let s = get(&state, "a");
        assert!(s.dialog.is_empty());
    }

    #[test]
    fn mark_session_boundary_idempotent_on_trailing_separator() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), separator_entry(20)]);
        let changed = state.mark_session_boundary("a", 100);
        assert!(!changed);
        let s = get(&state, "a");
        assert_eq!(s.dialog.len(), 2);
    }

    #[test]
    fn mark_session_boundary_missing_session_is_noop() {
        let state = AppState::new();
        assert!(!state.mark_session_boundary("nope", 100));
    }

    #[test]
    fn mark_session_boundary_clears_stale_input_tokens() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), assistant_entry("a1", 20)]);
        state.sessions.lock().unwrap()[0].input_tokens = Some(160_000);
        let changed = state.mark_session_boundary("a", 100);
        assert!(changed);
        // Compaction reduced the real context — the stale count must be dropped
        // so context_percent doesn't stay frozen at the pre-compact value.
        assert_eq!(get(&state, "a").input_tokens, None);
    }

    #[test]
    fn mark_session_boundary_clears_tokens_even_when_separator_is_noop() {
        let state = AppState::new();
        seed(&state, vec![user_entry("u1", 10), separator_entry(20)]);
        state.sessions.lock().unwrap()[0].input_tokens = Some(160_000);
        // The dialog already ends in a separator, so no separator is appended,
        // but the compaction still cleared the context — tokens must reset and
        // the change must be reported so the reconciler re-evaluates.
        let changed = state.mark_session_boundary("a", 100);
        assert!(changed);
        let s = get(&state, "a");
        assert_eq!(s.input_tokens, None);
        assert_eq!(s.updated, 100);
        assert_eq!(s.dialog.len(), 2);
    }

    #[test]
    fn continuation_only_applies_to_task_boundary_transitions() {
        let state = AppState::new();
        let cont: Vec<String> = vec!["go".into()];
        // Existing approval cycle: blocked → working with label "go".
        // This isn't a task boundary regardless of the continuation list,
        // so the rule is a no-op here — original_prompt is still pinned.
        state.apply_set(set("a", Status::Working, "fix foo.py"), 0, &cont, None);
        state.apply_set(set("a", Status::Blocked, "permission?"), 1000, &cont, None);
        state.apply_set(set("a", Status::Working, "go"), 2000, &cont, None);
        let s = get(&state, "a");
        assert_eq!(s.original_prompt.as_deref(), Some("fix foo.py"));
    }

    // -------- merge_dialog_entries (transcript watcher) tests --------

    #[test]
    fn merge_replay_of_same_delta_is_noop() {
        let mut dialog = Vec::new();
        let delta = vec![user_entry("u1", 10), assistant_entry("a1", 20), separator_entry(30)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 3);
        // The watcher re-reads a stretch it has already read — must not duplicate.
        assert!(!merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 3);
    }

    #[test]
    fn merge_replaces_streamed_assistant_in_place() {
        let mut dialog = vec![user_entry("u1", 10), assistant_entry("partial", 20)];
        // The watcher saw the same-turn assistant text grow and restamped it;
        // the re-read carries the newer version.
        let delta = vec![assistant_entry("final", 25)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 2);
        assert_eq!(dialog[1].text, "final");
        assert_eq!(dialog[1].timestamp, 25);
    }

    #[test]
    fn merge_appends_assistant_after_user_boundary() {
        let mut dialog = vec![user_entry("u1", 10), assistant_entry("a1", 20)];
        let delta = vec![user_entry("u2", 30), assistant_entry("a2", 40)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        assert_eq!(dialog.len(), 4);
        assert_eq!(dialog[3].text, "a2");
    }

    #[test]
    fn merge_separator_skips_when_dialog_ends_with_one() {
        let mut dialog = vec![user_entry("u1", 10), separator_entry(20)];
        assert!(!merge_dialog_entries(&mut dialog, &[separator_entry(50)]));
        assert_eq!(dialog.len(), 2);
    }

    #[test]
    fn merge_user_dedups_unanswered_reread() {
        let mut dialog = vec![user_entry("fix bug", 10)];
        // Same prompt re-read with a different timestamp while it is still the
        // unanswered tail (a plain transcript re-read) — text dedup
        // against the tail catches it.
        assert!(!merge_dialog_entries(&mut dialog, &[user_entry("fix bug", 30)]));
        assert_eq!(dialog.len(), 1);
    }

    #[test]
    fn merge_keeps_prompt_repeated_after_a_reply() {
        // The approval loop — "y" / "retry" / "all" one turn later is a genuine
        // new turn, not a re-read. Dropping it also cost the reply *before* it:
        // with the turn boundary gone, the next assistant overwrote that reply
        // in place, so two entries vanished below the newest one.
        let mut dialog = vec![user_entry("y", 10), assistant_entry("first reply", 20)];
        let delta = vec![user_entry("y", 30), assistant_entry("second reply", 40)];
        assert!(merge_dialog_entries(&mut dialog, &delta));
        let texts: Vec<&str> = dialog.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["y", "first reply", "y", "second reply"], "both turns survive intact");
    }

    #[test]
    fn merge_assistant_stops_at_a_separator() {
        // A reply arriving first after a /clear or compact boundary must append,
        // not reach back across the separator and overwrite the previous one.
        let mut dialog = vec![user_entry("u1", 10), assistant_entry("before", 20), separator_entry(30)];
        assert!(merge_dialog_entries(&mut dialog, &[assistant_entry("after", 40)]));
        assert_eq!(dialog.len(), 4);
        assert_eq!(dialog[1].text, "before", "pre-boundary reply untouched");
        assert_eq!(dialog[3].text, "after");
    }

    #[test]
    fn merge_preserves_incoming_metadata() {
        let mut dialog = Vec::new();
        let mut entry = user_entry("u1", 42);
        entry.task_start = true;
        assert!(merge_dialog_entries(&mut dialog, &[entry]));
        assert_eq!(dialog[0].timestamp, 42, "the transcript's timestamps survive");
        assert!(dialog[0].task_start, "task boundary flag survives");
    }

    // -------- remote-device storage tests --------

    fn remote_session(id: &str, origin: &str) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status: Status::Working,
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "claude".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            waiting_backstop_armed: false,
            display_name: None,
            origin: Some(origin.to_string()),
            instruction_drift: false,
            terminal_stale_at: None,
            canary: Canary::Off,
            attended_at: None,
            content_seen_at: None,
            origin_label: None,
            turn_from_relay: false,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            subagent_gate: None,
        }
    }

    #[test]
    fn remote_snapshot_is_ordered_by_device_name() {
        let state = AppState::new();
        let mut remote = state.remote.lock().unwrap();
        remote.insert("zeta".into(), RemoteDevice { sessions: vec![remote_session("zeta/p", "zeta")], last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        remote.insert("alpha".into(), RemoteDevice { sessions: vec![remote_session("alpha/p", "alpha")], last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        drop(remote);
        let snap = state.remote_snapshot();
        let ids: Vec<&str> = snap.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha/p", "zeta/p"], "stable order across emits");
    }

    #[test]
    fn reap_remote_drops_only_silent_devices() {
        let state = AppState::new();
        let mut remote = state.remote.lock().unwrap();
        remote.insert("fresh".into(), RemoteDevice { sessions: Vec::new(), last_seen: 1000, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        remote.insert("stale".into(), RemoteDevice { sessions: Vec::new(), last_seen: 0, origin_addr: String::new(), registry_sessions: None, identity: crate::tailnet::Attestation::Claimed });
        drop(remote);
        assert!(state.reap_remote(1500, 1000), "stale device dropped");
        assert!(state.remote.lock().unwrap().contains_key("fresh"));
        assert!(!state.remote.lock().unwrap().contains_key("stale"));
        assert!(!state.reap_remote(1500, 1000), "second reap is a no-op");
    }

    // -------- subagent permission prompts --------

    fn prompt_from(agent: &str, tool: &str) -> SubagentPromptRequest {
        SubagentPromptRequest { agent_id: agent.into(), session_id: "sess".into(), agent_type: None, tool_name: tool.into(), tool_input: serde_json::Value::Null, label: format!("needs approval: {tool}"), subagents_dir: None }
    }

    /// What `http_server` hands over for a subagent's `PermissionRequest`.
    fn open(state: &AppState, id: &str, agent: &str, tool: &str, at: i64) -> OpenOutcome {
        state.open_subagent_prompt(set(id, Status::Blocked, &format!("needs approval: {tool}")), prompt_from(agent, tool), at, None)
    }

    fn gate_of(state: &AppState, id: &str) -> Option<SubagentGate> {
        get(state, id).subagent_gate
    }

    fn waiting_armed(id: &str, label: &str) -> SetInput {
        let mut input = set(id, Status::Waiting, label);
        input.waiting_backstop_armed = true;
        input
    }

    #[test]
    fn a_subagent_prompt_blocks_the_row_over_its_base() {
        let state = AppState::new();
        state.apply_set(waiting_armed("a", "run tests"), 1_000, NO_CONTINUATIONS, None);
        let o = open(&state, "a", "agent-1", "Bash", 5_000);
        assert_eq!((o.request, o.pending, o.base_status, o.dialog_changed), (1, 1, Status::Waiting, false));

        let s = get(&state, "a");
        assert_eq!(s.status, Status::Blocked);
        assert_eq!(s.label, "needs approval: Bash");
        assert!(!s.waiting_backstop_armed, "a blocked row is never time-settled");
        assert_eq!(s.state_entered_at, 5_000);
        assert_eq!(s.updated, 5_000);
        let gate = s.subagent_gate.expect("gate");
        assert_eq!(gate.base, BaseState { status: Status::Waiting, label: "run tests".into(), waiting_backstop_armed: true, state_entered_at: 1_000 });
        assert_eq!(gate.blocked_since, 5_000);
    }

    #[test]
    fn releasing_the_last_prompt_restores_the_base_verbatim() {
        let state = AppState::new();
        state.apply_set(waiting_armed("a", "run tests"), 1_000, NO_CONTINUATIONS, None);
        let o = open(&state, "a", "agent-1", "Bash", 5_000);
        let settled = state.settle_subagent_prompts("a", SettleScope::Request(o.request), 9_000).expect("settled");
        assert!(settled.released);
        assert_eq!((settled.remaining, settled.status), (0, Status::Waiting));

        let s = get(&state, "a");
        assert_eq!(s.status, Status::Waiting);
        assert_eq!(s.label, "run tests");
        assert!(s.waiting_backstop_armed);
        assert_eq!(s.state_entered_at, 1_000, "the WAIT's own clock, not the release's");
        assert_eq!(s.updated, 9_000);
        assert!(s.subagent_gate.is_none());
    }

    #[test]
    fn a_second_prompt_joins_without_resnapshotting() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        let first = open(&state, "a", "agent-1", "Bash", 2_000);
        let second = open(&state, "a", "agent-2", "Edit", 3_000);
        assert_eq!((second.pending, second.base_status), (2, Status::Working));
        let s = get(&state, "a");
        assert_eq!(s.label, "needs approval: Edit", "the newest prompt names the row");
        assert_eq!(s.state_entered_at, 2_000, "one BLOCK, one clock");
        assert_eq!(gate_of(&state, "a").unwrap().base.state_entered_at, 1_000, "the base was not re-captured over the first BLOCK");

        let partial = state.settle_subagent_prompts("a", SettleScope::Request(second.request), 4_000).expect("settled");
        assert!(!partial.released);
        assert_eq!((partial.remaining, partial.status), (1, Status::Blocked));
        assert_eq!(get(&state, "a").label, "needs approval: Bash", "falls back to the prompt still open");

        state.settle_subagent_prompts("a", SettleScope::Request(first.request), 5_000).expect("settled");
        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Working, "fix foo", 1_000));
    }

    #[test]
    fn two_prompts_from_one_agent_are_released_separately() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        let r1 = open(&state, "a", "agent-1", "Bash", 1_000).request;
        let r2 = open(&state, "a", "agent-1", "Bash", 1_100).request;
        assert_ne!(r1, r2, "a retry or a parallel call is its own prompt");

        let o = state.settle_subagent_prompts("a", SettleScope::Request(r1), 2_000).expect("settled");
        assert_eq!((o.settled.len(), o.remaining, o.status), (1, 1, Status::Blocked));
        assert!(state.settle_subagent_prompts("a", SettleScope::Request(r1), 2_100).is_none(), "one result never releases twice");
        assert!(state.settle_subagent_prompts("a", SettleScope::Request(r2), 3_000).expect("settled").released);
    }

    #[test]
    fn settling_by_agent_releases_only_that_agents_prompts() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 1_000);
        open(&state, "a", "agent-2", "Edit", 1_100);
        open(&state, "a", "agent-1", "Write", 1_200);

        let o = state.settle_subagent_prompts("a", SettleScope::Agent("agent-1"), 2_000).expect("settled");
        assert_eq!((o.settled.len(), o.remaining, o.released), (2, 1, false));
        assert_eq!(get(&state, "a").label, "needs approval: Edit");
        assert!(state.settle_subagent_prompts("a", SettleScope::Agent("agent-1"), 2_100).is_none());
    }

    #[test]
    fn settling_a_session_releases_every_prompt_it_raised_once() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 1_000);
        open(&state, "a", "agent-2", "Edit", 1_100);
        let o = state.settle_subagent_prompts("a", SettleScope::Session("sess"), 2_000).expect("settled");
        assert_eq!((o.settled.len(), o.remaining, o.released, o.status), (2, 0, true, Status::Working));
        assert!(state.settle_subagent_prompts("a", SettleScope::Session("sess"), 2_100).is_none(), "nothing left to release");
    }

    #[test]
    fn a_sibling_sessions_stop_leaves_another_sessions_prompt_open() {
        // Two instances share row "a" (a `--fork-session --resume` migration). The
        // forked one's subagent is on a dialog; the other finishing proves nothing.
        let state = AppState::new();
        state.apply_set(set("a", Status::Waiting, "task"), 0, NO_CONTINUATIONS, None);
        let forked = SubagentPromptRequest { session_id: "forked".into(), ..prompt_from("agent-1", "Bash") };
        state.open_subagent_prompt(set("a", Status::Blocked, "needs approval: Bash"), forked, 1_000, None);
        assert!(state.settle_subagent_prompts("a", SettleScope::Session("sess"), 2_000).is_none());
        assert_eq!(get(&state, "a").status, Status::Blocked);
        assert!(state.settle_subagent_prompts("a", SettleScope::Session("forked"), 3_000).expect("settled").released);
    }

    #[test]
    fn settling_an_unknown_request_changes_nothing() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 5_000);
        assert!(state.settle_subagent_prompts("a", SettleScope::Request(999), 9_000).is_none());
        assert!(state.settle_subagent_prompts("nope", SettleScope::Session("sess"), 9_000).is_none());
        let s = get(&state, "a");
        assert_eq!((s.status, s.updated), (Status::Blocked, 5_000), "a no-op settle leaves `updated`, the reaper's guard, alone");
    }

    #[test]
    fn a_main_agent_set_under_the_gate_moves_only_the_base() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 5_000);
        // The main turn's `Stop` arrives while the subagent's dialog is open.
        state.apply_set(set_no_label("a", Status::Done), 7_000, NO_CONTINUATIONS, None);

        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Blocked, "needs approval: Bash", 5_000), "still BLOCK");
        assert_eq!(s.working_accumulated_ms, 6_000, "banked against the base's own Working clock");
        assert_eq!(s.updated, 7_000);
        assert_eq!(gate_of(&state, "a").unwrap().base, BaseState { status: Status::Done, label: "fix foo".into(), waiting_backstop_armed: false, state_entered_at: 7_000 });

        state.settle_subagent_prompts("a", SettleScope::Session("sess"), 9_000).expect("settled");
        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Done, "fix foo", 7_000), "Done, on Done's own clock");
    }

    #[test]
    fn a_main_prompt_under_the_gate_is_what_the_release_reveals() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix foo"), 1_000, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 5_000);
        state.apply_set(set("a", Status::Blocked, "needs approval: Write"), 6_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").label, "needs approval: Bash", "the subagent's prompt still names the row");

        state.settle_subagent_prompts("a", SettleScope::Session("sess"), 8_000).expect("settled");
        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Blocked, "needs approval: Write", 6_000));
    }

    #[test]
    fn entering_working_under_the_gate_records_the_base_as_revert_target() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "first task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set_no_label("a", Status::Done), 1_000, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 2_000);
        state.apply_set(set("a", Status::Working, "second task"), 3_000, NO_CONTINUATIONS, None);

        let s = get(&state, "a");
        assert_eq!(s.status_before_working, Status::Done, "the main agent's status, never the overlay's Blocked");
        assert_eq!(s.original_prompt.as_deref(), Some("second task"), "Done -> Working is a task boundary on the base");
        assert_eq!(s.working_accumulated_ms, 0);
        assert_eq!(s.status, Status::Blocked);
    }

    #[test]
    fn an_esc_under_the_gate_reverts_the_base_and_keeps_the_block() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "fix the parser"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Blocked, "Push?"), 1_000, NO_CONTINUATIONS, None);
        state.apply_set(set("a", Status::Working, "ny"), 2_000, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 3_000);

        assert_eq!(state.revert_cancelled_turn("a", 4_000), Some((Status::Blocked, true)));
        let s = get(&state, "a");
        assert_eq!((s.status, s.state_entered_at, s.label.as_str()), (Status::Blocked, 3_000, "needs approval: Bash"), "the BLOCK stands");
        assert_eq!(gate_of(&state, "a").unwrap().base.status, Status::Blocked, "the main agent is back on its question");
        assert_eq!(gate_of(&state, "a").unwrap().base.state_entered_at, 4_000);
    }

    #[test]
    fn the_block_clock_is_continuous_when_the_row_was_already_blocked() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Blocked, "has a question"), 1_000, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 5_000);
        assert_eq!(get(&state, "a").state_entered_at, 1_000);
        assert_eq!(gate_of(&state, "a").unwrap().blocked_since, 1_000);
    }

    #[test]
    fn a_prompt_for_an_unknown_row_creates_it_blocked_over_done() {
        // `Done`, the neutral sink — all this row's existence proves is that some
        // main agent launched a subagent that asked for permission. `Idle` would
        // claim the user has nothing to come back to, which nothing established.
        let state = AppState::new();
        let o = open(&state, "a", "agent-1", "Bash", 5_000);
        assert_eq!((o.pending, o.base_status, o.dialog_changed), (1, Status::Done, false));
        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Blocked, "needs approval: Bash", 5_000));
        assert_eq!(s.original_prompt, None, "a subagent's dialog is not a task");

        // The base is written back verbatim, clock included — so what the row
        // shows once the dialog releases is the same neutral `Done` it was
        // invented with, on the prompt-open clock.
        state.settle_subagent_prompts("a", SettleScope::Session("sess"), 6_000).expect("settled");
        let s = get(&state, "a");
        assert_eq!((s.status, s.label.as_str(), s.state_entered_at), (Status::Done, "", 5_000));

        let restored = PersistedSession { delegated_task: None, message_line: None, dialog: vec![user_entry("old", 10)], original_prompt: None, task_started_at: 0 };
        let o = state.open_subagent_prompt(set("b", Status::Blocked, "needs approval: Bash"), prompt_from("agent-1", "Bash"), 7_000, Some(restored));
        assert!(o.dialog_changed, "a restored history is persisted like any new row's");
    }

    #[test]
    fn a_gated_waiting_base_is_not_time_settled() {
        let state = AppState::new();
        state.apply_set(waiting_armed("a", "dev server"), 0, NO_CONTINUATIONS, None);
        open(&state, "a", "agent-1", "Bash", 1_000);
        assert!(!state.settle_stale_waiting("a", 1_000, 900_000), "the row reads Blocked");
        assert_eq!(get(&state, "a").status, Status::Blocked);

        state.settle_subagent_prompts("a", SettleScope::Session("sess"), 950_000).expect("settled");
        assert!(state.settle_stale_waiting("a", 950_000, 960_000), "the restored WAIT resumes its own count");
        assert_eq!(get(&state, "a").status, Status::Done);
    }

    #[test]
    fn base_status_reads_through_the_gate() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "a").base_status(), Status::Working);
        open(&state, "a", "agent-1", "Bash", 1_000);
        let s = get(&state, "a");
        assert_eq!((s.status, s.base_status()), (Status::Blocked, Status::Working));
        assert!(s.base_status().is_live_work(), "a workflow under a dialog still keeps the Mac awake");
    }

    #[test]
    fn the_gate_never_reaches_the_wire() {
        let state = AppState::new();
        open(&state, "a", "agent-1", "Bash", 1_000);
        let json = serde_json::to_value(get(&state, "a")).expect("serialize");
        assert!(json.get("subagent_gate").is_none(), "the overlay is this process's bookkeeping");
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("blocked"), "the wire carries what the row shows");
    }

    #[test]
    fn removing_a_row_drops_its_prompts() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        let r = open(&state, "a", "agent-1", "Bash", 1_000).request;
        assert!(state.take_session("a", None, BoundaryKind::Clear, 2_000).is_some());
        assert!(state.pending_subagent_prompts().is_empty());

        state.apply_set(set_no_label("a", Status::Idle), 3_000, NO_CONTINUATIONS, None);
        assert!(state.settle_subagent_prompts("a", SettleScope::Request(r), 4_000).is_none(), "the recreated row owes nothing");
        assert_eq!(get(&state, "a").status, Status::Idle);
    }

    #[test]
    fn pending_subagent_prompts_lists_every_open_request() {
        let state = AppState::new();
        state.apply_set(set("a", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        state.apply_set(set("b", Status::Working, "task"), 0, NO_CONTINUATIONS, None);
        let r1 = open(&state, "a", "agent-1", "Bash", 1_000).request;
        let r2 = open(&state, "a", "agent-2", "Edit", 1_100).request;
        let r3 = open(&state, "b", "agent-3", "Write", 1_200).request;
        let listed: Vec<(String, u64, String)> = state.pending_subagent_prompts().into_iter().map(|(id, p)| (id, p.request, p.prompt.agent_id)).collect();
        assert_eq!(listed, vec![("a".into(), r1, "agent-1".into()), ("a".into(), r2, "agent-2".into()), ("b".into(), r3, "agent-3".into())]);
    }

    /// A row `status`, with `label`, `prompt` and `dialog` set as given.
    fn row(status: Status, label: &str, prompt: Option<&str>, dialog: Vec<DialogEntry>) -> AgentSession {
        let state = AppState::new();
        state.apply_set(set("r", status, label), 0, NO_CONTINUATIONS, None);
        let mut s = get(&state, "r");
        s.label = label.to_string();
        s.original_prompt = prompt.map(str::to_string);
        s.dialog = dialog;
        s
    }

    fn entry(role: DialogRole, text: &str, task_start: bool) -> DialogEntry {
        DialogEntry { role, text: text.to_string(), timestamp: 0, status: Status::Done, task_start, boundary: None }
    }

    #[test]
    fn primary_text_is_the_question_while_blocked_and_the_task_otherwise() {
        assert_eq!(row(Status::Blocked, "needs approval: Bash", Some("Fix the build"), Vec::new()).primary_text(), "needs approval: Bash");
        assert_eq!(row(Status::Error, "rate limited", Some("Fix the build"), Vec::new()).primary_text(), "rate limited");
        assert_eq!(row(Status::Done, "needs approval: Bash", Some("Fix the build"), Vec::new()).primary_text(), "Fix the build", "the stale question a Stop kept is not shown");
        assert_eq!(row(Status::Working, "thinking", None, Vec::new()).primary_text(), "thinking");
    }

    #[test]
    fn a_stored_hand_back_is_no_task() {
        // Rows persisted before hand-backs stopped starting a task still carry one as their prompt.
        let hand_back = "<agent-message from=\"ab070651cd45c459e\"> [Subagent hand-back] The report. </agent-message>";
        let s = row(Status::Done, "", Some(hand_back), Vec::new());
        assert_eq!(s.person_task(), None);
        assert_eq!(s.primary_text(), "");
        // A sender's hand-back resolved into another row's delegated task before resolution refused one.
        let mut d = row(Status::Done, "", Some("<cross-session-message from=\"uds:x\" from-name=\"y\"> hi </cross-session-message>"), Vec::new());
        d.delegated_task = Some(hand_back.to_string());
        assert_eq!(d.person_task(), None);
        assert!(!d.primary_text().contains("agent-message"));
    }

    fn current(t: &str) -> Option<RowLine> {
        Some(RowLine::Current(t.to_string()))
    }

    fn past(t: &str) -> Option<RowLine> {
        Some(RowLine::Past(t.to_string()))
    }

    #[test]
    fn row_line_is_the_primary_text_when_there_is_one() {
        let dialog = vec![entry(DialogRole::User, "Older task", true)];
        assert_eq!(row(Status::Done, "", Some("Fix the build"), dialog.clone()).row_line(), current("Fix the build"));
        assert_eq!(row(Status::Blocked, "needs approval: Bash", Some("Fix the build"), dialog.clone()).row_line(), current("needs approval: Bash"));
        // Only an empty primary text falls back.
        assert_eq!(row(Status::Done, "", Some("  "), dialog).row_line(), current("  "));
    }

    #[test]
    fn row_line_falls_back_to_the_last_task_start() {
        let dialog = vec![entry(DialogRole::User, "First task", true), entry(DialogRole::Assistant, "Done.", false), entry(DialogRole::User, "Second task", true), entry(DialogRole::User, "and tests", false), entry(DialogRole::Separator, "", false)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), past("Second task"));
    }

    #[test]
    fn row_line_without_a_task_start_prefers_a_substantive_prompt_over_an_approval() {
        let dialog = vec![entry(DialogRole::User, "Rename the module", false), entry(DialogRole::User, "ok", false), entry(DialogRole::User, "   ", false)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), past("Rename the module"));
        // Counted in UTF-16 units after trimming.
        let dialog = vec![entry(DialogRole::User, "Fixes", false), entry(DialogRole::User, " 😀😀 ", false)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), past("Fixes"), "two emoji are four units, so not substantive");
    }

    #[test]
    fn row_line_falls_back_to_any_prompt_then_any_entry_then_nothing() {
        let dialog = vec![entry(DialogRole::User, "yes", false), entry(DialogRole::Assistant, "Merged.", false)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), past("yes"));
        let dialog = vec![entry(DialogRole::Assistant, "Restored reply", false), entry(DialogRole::Separator, "---", false)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), past("Restored reply"), "a separator is never the row's text");
        assert_eq!(row(Status::Idle, "", None, Vec::new()).row_line(), None);
        let dialog = vec![entry(DialogRole::User, "Older prompt", false), entry(DialogRole::User, "", true)];
        assert_eq!(row(Status::Idle, "", None, dialog).row_line(), None, "an empty task start is no text, and is not passed over");
    }

    #[test]
    fn a_row_line_is_tagged_by_kind_on_the_wire() {
        assert_eq!(serde_json::to_value(RowLine::Past("Fix it".into())).unwrap(), serde_json::json!({ "kind": "past", "text": "Fix it" }));
        assert_eq!(serde_json::to_value(RowLine::Current("Fix it".into())).unwrap(), serde_json::json!({ "kind": "current", "text": "Fix it" }));
    }

    // -------- delegated_task --------

    /// A `SendMessage` envelope in the whitespace-collapsed form a row keeps as
    /// its `label` and `original_prompt`, the body cut to a placeholder.
    const ENVELOPE: &str = r#"<cross-session-message from="uds:\\.\pipe\LOCAL\cc-msg-e4b10093983d402af357640b6ef02c74" from-name="agwinterm-sidebar-label-ownership" from-mode="prompting"> placeholder message </cross-session-message>"#;

    fn delegated(label: &str, task: Option<&str>) -> SetInput {
        SetInput { delegated_task: task.map(str::to_string), message_is_reply: crate::peer_message::parse_agent_message(label).map(|m| m.is_reply()), ..set("r", Status::Working, label) }
    }

    #[test]
    fn a_delegated_task_is_what_the_row_and_its_ping_show() {
        let mut s = row(Status::Working, ENVELOPE, Some(ENVELOPE), Vec::new());
        s.delegated_task = Some("add a title-bar caption to agwinterm".into());
        assert_eq!(s.row_line(), current("add a title-bar caption to agwinterm"));
        assert_eq!(s.primary_text(), "add a title-bar caption to agwinterm");
        s.status = Status::Done;
        assert_eq!(s.row_line(), current("add a title-bar caption to agwinterm"), "and after the turn ends");
        s.status = Status::Blocked;
        s.label = "needs approval: Bash".into();
        assert_eq!(s.row_line(), current("needs approval: Bash"), "a row asking something still shows the question");
    }

    /// A row persisted before `delegated_task` existed, or one whose message
    /// arrived with no task recorded, still never shows the envelope.
    #[test]
    fn an_envelope_without_a_delegated_task_shows_its_message_never_the_envelope() {
        assert_eq!(row(Status::Working, ENVELOPE, Some(ENVELOPE), Vec::new()).row_line(), current("placeholder message"));
        assert_eq!(row(Status::Working, ENVELOPE, None, Vec::new()).row_line(), current("placeholder message"), "the label is unwrapped too");
        let raw = "<cross-session-message from=\"uds:/tmp/cc-socks/1.sock\" from-name=\"n\" from-mode=\"prompting\">\nfirst line\nsecond line\n</cross-session-message>";
        assert_eq!(row(Status::Idle, "", None, vec![entry(DialogRole::User, raw, true)]).row_line(), past("first line"), "a past task read back out of the dialog");
        let empty = "<cross-session-message from=\"uds:x\" from-name=\"n\" from-mode=\"prompting\">\n</cross-session-message>";
        assert_eq!(row(Status::Idle, "", None, vec![entry(DialogRole::User, "an older real prompt", false), entry(DialogRole::User, empty, false)]).row_line(), past("an older real prompt"), "an empty message is blank, and passed over like one");
    }

    #[test]
    fn an_agent_message_mid_turn_opens_no_task() {
        for prior in [Status::Working, Status::Waiting] {
            let state = AppState::new();
            state.apply_set(set("r", Status::Working, "fix the parser"), 1_000, NO_CONTINUATIONS, None);
            state.apply_set(set_no_label("r", prior), 2_000, NO_CONTINUATIONS, None);
            state.apply_set(SetInput { dialog_entry: Some(PendingDialogEntry { role: DialogRole::User, text: ENVELOPE.into() }), ..delegated(ENVELOPE, Some("some other agent's task")) }, 3_000, NO_CONTINUATIONS, None);
            let s = get(&state, "r");
            assert_eq!((s.original_prompt.as_deref(), s.delegated_task.as_deref(), s.task_started_at), (Some("fix the parser"), None, 1_000), "{prior:?}: the person's task stands");
            assert!(!s.dialog.last().expect("the message").task_start, "{prior:?}: the message is no task start");
        }
    }

    /// A relayed reply to this row's own message, in the collapsed form a row
    /// keeps as its `label`, the body cut to a placeholder.
    fn relayed_reply() -> String {
        let r = crate::peer_message::Relayed { origin_device: "air", from_agent: "x", from_label: None, from_task: Some("some other agent's task"), text: "placeholder answer", reply_to: Some("air/x"), message_id: "air-1-1", in_reply_to: Some("chrome-1-0"), reply_port: 9077, attestation: crate::tailnet::Attestation::Claimed, tailnet_user: None };
        crate::peer_message::build_content(&r).split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_reply_to_this_rows_message_opens_no_task_on_a_finished_row() {
        for prior in [Status::Done, Status::Idle, Status::Working] {
            let state = AppState::new();
            state.apply_set(set("r", Status::Working, "ask the mac to update"), 1_000, NO_CONTINUATIONS, None);
            state.apply_set(set_no_label("r", prior), 2_000, NO_CONTINUATIONS, None);
            state.apply_set(delegated(&relayed_reply(), Some("some other agent's task")), 3_000, NO_CONTINUATIONS, None);
            let s = get(&state, "r");
            assert_eq!((s.original_prompt.as_deref(), s.delegated_task.as_deref(), s.task_started_at), (Some("ask the mac to update"), None, 1_000), "{prior:?}: the person's task stands");
        }
    }

    #[test]
    fn an_agent_message_on_a_finished_row_starts_a_task() {
        for prior in [Status::Done, Status::Idle] {
            let state = AppState::new();
            state.apply_set(set("r", Status::Working, "fix the parser"), 1_000, NO_CONTINUATIONS, None);
            state.apply_set(set_no_label("r", prior), 2_000, NO_CONTINUATIONS, None);
            state.apply_set(delegated(ENVELOPE, Some("rename the tray item")), 3_000, NO_CONTINUATIONS, None);
            let s = get(&state, "r");
            assert_eq!((s.original_prompt.as_deref(), s.delegated_task.as_deref(), s.task_started_at), (Some(ENVELOPE), Some("rename the tray item"), 3_000), "{prior:?}");
        }
    }

    #[test]
    fn a_delegated_task_moves_only_with_the_prompt_it_stands_for() {
        let state = AppState::new();
        state.apply_set(delegated(ENVELOPE, Some("rename the tray item")), 1_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "r").delegated_task.as_deref(), Some("rename the tray item"), "captured with the prompt on a new row");
        assert_eq!(get(&state, "r").original_prompt.as_deref(), Some(ENVELOPE), "the envelope itself is kept");

        state.apply_set(set("r", Status::Blocked, "has a question"), 2_000, NO_CONTINUATIONS, None);
        state.apply_set(delegated(ENVELOPE, Some("some other agent's task")), 3_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "r").delegated_task.as_deref(), Some("rename the tray item"), "an answer to a question is not a new task");

        state.apply_set(set_no_label("r", Status::Done), 4_000, NO_CONTINUATIONS, None);
        state.apply_set(set("r", Status::Working, "fix the parser"), 5_000, NO_CONTINUATIONS, None);
        let s = get(&state, "r");
        assert_eq!((s.original_prompt.as_deref(), s.delegated_task), (Some("fix the parser"), None), "a person's next task clears it");
    }

    #[test]
    fn a_restored_row_brings_its_delegated_task_back_unless_a_boundary_ended_it() {
        let persisted = |dialog| PersistedSession { dialog, original_prompt: Some(ENVELOPE.into()), delegated_task: Some("ship the hero shot".into()), message_line: None, task_started_at: 10 };
        let state = AppState::new();
        state.apply_set(set_no_label("r", Status::Done), 1_000, NO_CONTINUATIONS, Some(persisted(vec![user_entry(ENVELOPE, 10)])));
        assert_eq!(get(&state, "r").delegated_task.as_deref(), Some("ship the hero shot"));

        let state = AppState::new();
        let ended = vec![user_entry(ENVELOPE, 10), DialogEntry { role: DialogRole::Separator, text: String::new(), timestamp: 20, status: Status::Done, task_start: false, boundary: Some(BoundaryKind::Clear) }];
        state.apply_set(set_no_label("r", Status::Done), 1_000, NO_CONTINUATIONS, Some(persisted(ended)));
        assert_eq!(get(&state, "r").delegated_task, None, "dropped with the prompt");

        let state = AppState::new();
        state.apply_set(set("r", Status::Working, "a person's prompt"), 1_000, NO_CONTINUATIONS, Some(persisted(vec![user_entry(ENVELOPE, 10)])));
        assert_eq!(get(&state, "r").delegated_task, None, "a prompt arriving with the event does not inherit the restored one");
    }

    /// The field rides `AgentSession` on the sync wire and `PersistedSession` in
    /// `prompt_history.json`; both must still read what an older build wrote.
    #[test]
    fn delegated_task_round_trips_and_is_optional_on_the_wire_and_on_disk() {
        let mut s = row(Status::Working, ENVELOPE, Some(ENVELOPE), Vec::new());
        s.delegated_task = Some("fix the parser".into());
        let mut json = serde_json::to_value(&s).unwrap();
        let back: AgentSession = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back.delegated_task.as_deref(), Some("fix the parser"));
        json.as_object_mut().unwrap().remove("delegated_task");
        let older: AgentSession = serde_json::from_value(json).expect("a push from an older peer parses");
        assert_eq!(older.delegated_task, None);

        let p = PersistedSession { dialog: Vec::new(), original_prompt: Some(ENVELOPE.into()), delegated_task: Some("fix the parser".into()), message_line: Some("a line".into()), task_started_at: 1 };
        let back: PersistedSession = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!((back.delegated_task.as_deref(), back.message_line.as_deref()), (Some("fix the parser"), Some("a line")));
        let older: PersistedSession = serde_json::from_str(r#"{"dialog":[],"original_prompt":"fix foo","task_started_at":1}"#).expect("an older prompt_history.json loads");
        assert_eq!((older.delegated_task, older.message_line), (None, None));

        let mut s = row(Status::Working, ENVELOPE, Some(ENVELOPE), Vec::new());
        s.message_line = Some("a line".into());
        let mut json = serde_json::to_value(&s).unwrap();
        assert_eq!(serde_json::from_value::<AgentSession>(json.clone()).unwrap().message_line.as_deref(), Some("a line"));
        json.as_object_mut().unwrap().remove("message_line");
        assert_eq!(serde_json::from_value::<AgentSession>(json).expect("a push from an older peer parses").message_line, None);
    }

    // -------- message_line --------

    /// The message's own line stands in on every display surface where the
    /// sender's task could not be had, and is kept apart from the task: neither
    /// the chain nor the relay is handed it, so the line is never reported
    /// onward as this row's task.
    #[test]
    fn a_message_line_is_shown_but_never_passed_on_as_the_row_s_task() {
        let mut s = row(Status::Working, ENVELOPE, Some(ENVELOPE), Vec::new());
        s.message_line = Some("Please commit your two files".into());
        assert_eq!(s.row_line(), current("Please commit your two files"));
        assert_eq!(s.primary_text(), "Please commit your two files");
        assert_eq!(s.person_task(), None, "nobody is told the line is a task");
        s.delegated_task = Some("add a title-bar caption".into());
        assert_eq!(s.person_task(), Some("add a title-bar caption"));
        let typed = row(Status::Working, "fix the parser", Some("fix the parser"), Vec::new());
        assert_eq!(typed.person_task(), Some("fix the parser"));
    }

    /// The hover tooltip lists the row's tasks from the same rule the task line
    /// uses: the task an agent message began reads as the sender's task, an
    /// older one as its message's first line, a typed one as typed, and the
    /// envelope the dialog keeps for the history window never shows.
    #[test]
    fn the_tooltip_tasks_never_show_an_envelope() {
        let raw_envelope = ENVELOPE.replace("> placeholder message <", ">\nplaceholder message\nsecond line\n<");
        let older = r#"<cross-session-message from="uds:\\.\pipe\LOCAL\cc-msg-0" from-name="n" from-mode="prompting">
an older request
</cross-session-message>"#;
        let dialog = vec![user_entry("fix the parser\nand the tests", 10), user_entry(older, 20), user_entry(&raw_envelope, 30)];
        let mut s = row(Status::Working, ENVELOPE, Some(&crate::adapters::claude::clean_prompt(&raw_envelope)), dialog);
        s.delegated_task = Some("add a title-bar caption to agwinterm".into());
        let lines = s.task_lines();
        assert_eq!(lines, vec![TaskLine { at: 10, text: "fix the parser\nand the tests".into() }, TaskLine { at: 20, text: "an older request".into() }, TaskLine { at: 30, text: "add a title-bar caption to agwinterm".into() }]);
        s.delegated_task = None;
        s.message_line = Some("placeholder message".into());
        assert_eq!(s.task_lines()[2].text, "placeholder message", "an unresolved sender's message shows its line, as the task line does");
    }

    #[test]
    fn a_message_line_moves_and_restores_with_the_prompt_it_stands_for() {
        let state = AppState::new();
        state.apply_set(SetInput { message_line: Some("the first line".into()), ..set("r", Status::Working, ENVELOPE) }, 1_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "r").message_line.as_deref(), Some("the first line"));
        state.apply_set(set_no_label("r", Status::Done), 2_000, NO_CONTINUATIONS, None);
        state.apply_set(set("r", Status::Working, "fix the parser"), 3_000, NO_CONTINUATIONS, None);
        assert_eq!(get(&state, "r").message_line, None, "a person's next task clears it");

        let persisted = PersistedSession { dialog: vec![user_entry(ENVELOPE, 10)], original_prompt: Some(ENVELOPE.into()), delegated_task: None, message_line: Some("the first line".into()), task_started_at: 10 };
        let state = AppState::new();
        state.apply_set(set_no_label("r", Status::Done), 1_000, NO_CONTINUATIONS, Some(persisted));
        assert_eq!(get(&state, "r").message_line.as_deref(), Some("the first line"));
    }
}
