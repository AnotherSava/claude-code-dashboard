//! Giving a live session its row back after the dashboard restarts.
//!
//! A row is created by a hook event and by nothing else, so a session that was
//! running before this process started is invisible until it next *acts* — and a
//! session parked on a question cannot act, because it is waiting for the user.
//! Measured on this machine: nine live local sessions, one row, and a `✋` tab
//! sitting three hours unanswered with nothing in the widget to say so. Restarts
//! are routine (every deploy is one), so "empty until each agent speaks again"
//! is the resting state, not a transient.
//!
//! Two questions have to be answered to put a row back, and they have different
//! authorities. Asking either source the other's question is where this goes
//! wrong, so the split is the design:
//!
//! - **Is an agent alive in this directory?** Claude Code's own session registry
//!   ([`crate::session_registry`]), which is pid-backed and image-confirmed. Not
//!   the terminal: a tab *outlives* the agent in it — `session_launcher` starts
//!   sessions as `zsh -ilc 'claude; exec zsh -i'` precisely so the tab survives —
//!   so a tab-driven restore resurrects rows for agents that have exited, frozen
//!   on the glyph they died on, and the row would then be immortal (see below).
//! - **What was it doing, and had the user read it?** The terminal tab title,
//!   through [`crate::terminals::TerminalAdapter`]. Not the registry, whose
//!   `Activity` is `idle`/`busy` only and is deliberately never mapped onto a
//!   [`Status`] — `busy → Working` is *false* for a session parked on an
//!   `AskUserQuestion`, and `idle` flattens Done, Blocked and Error alike. Live
//!   proof: `what-is-next` reads `idle` in the registry while its tab reads `✋`.
//!
//! **The tab title is the only durable copy of a row's status.** Nothing on disk
//! holds it — `prompt_history.json` persists the dialog, the prompt and the task
//! clock, and no status — so reading the glyph back is not inference from a
//! proxy; it is this dashboard reading its own last published record from the one
//! place that outlives the process. It carries a second fact for free: titles are
//! written from `commands::display_snapshot`, i.e. *after* `apply_read_as_idle`,
//! so `🟢` means finished-and-unread while `⚪` means finished-and-read. That is
//! why nothing here restores `attended_at` — which is `#[serde(skip)]` by
//! deliberate design, this machine's observation of this keyboard — and nothing
//! needs to: the attention flip was already applied when the title was written,
//! so a read row comes back `Idle` and an unread one comes back `Done`, still
//! asking. A tab with no title we recognize is **skipped**, never guessed at, and
//! that same rule is what stops a removed row from being resurrected — the blank
//! `terminal_title::sync` writes on removal is exactly a title we do not
//! recognize.
//!
//! **`Working` and `Waiting` are the two statuses a title cannot be trusted for**,
//! and [`restored_status`] is where that is decided. They assert a turn is in
//! flight *right now*, and the glyph's staleness is the dashboard's downtime,
//! which is unbounded. So they alone are checked against the registry's coarse
//! reading and degrade to `Idle` without it. That is not a nicety: a phantom
//! `Working` row holds off macOS sleep through `lid_awake` (a system-wide switch
//! that also suppresses thermal-emergency sleep), reads blue forever on this
//! screen and on the peer's, and a phantom `Waiting` can never be settled because
//! `waiting_backstop_armed` does not survive a restart either.
//!
//! **A restored row is created outside every mechanism that exists to remove
//! one** — `AgentPids` (the reaper), `ChatIdRegistry::owners` (`clear_permitted`)
//! and `waiting_backstop_armed` are all in-memory and all hook-populated. The
//! reaper is the one that matters, since without it a row whose session exits
//! without a `SessionEnd` is immortal, so [`spawn`] records the owning pid the
//! registry names — but only where the registry names exactly one, mirroring
//! `terminal_title::tab_pid` and `session_registry::inbox_for`, both of which
//! refuse ambiguity rather than pick. With two sessions collapsed into one row,
//! recording the speaker's pid would let the sibling's death reap a row the other
//! is still using.
//!
//! **Two costs of the row being real, both accepted.** A restored row writes its
//! tab title like any other, and it has no `input_tokens` (those come from the
//! transcript watcher, which is started only from the hook path), so a tab that
//! read `✋ dash [87%]` becomes `✋ dash` — the context warning is erased and does
//! not return until the session's next turn. Not writing the title instead would
//! be worse: the row and the tab would then disagree about the same session, and
//! the tab's glyph is what the *next* restart reads. And the row has no watcher
//! at all until that turn, so no context percentage and no Esc-cancel detection
//! in the meantime; any user action supplies it, and a session parked on a
//! question has nothing in flight for either to observe.
//!
//! **This is a startup task, and it stops when it is done.** Not a reconcile on a
//! timer — that is what it was first written as, and the timer was defending a
//! recurring gap that does not exist: over ~90 days of `widget.jsonl` a row is
//! removed while its session still lives exactly never. The 162 `session_clear`s
//! are all `/clear`, which fires `SessionEnd` then `SessionStart` and recreates
//! the row immediately; the 4 `reap_exited`s are sessions that genuinely ended,
//! which this must not bring back; the 5 `clear_ignored`s were refusals that
//! removed nothing.
//!
//! What is real is the *startup* race: the dashboard and the terminal both start
//! at login in no fixed order, so a single pass at `Ready` routinely asks a
//! terminal that is not up yet. That calls for retrying until both oracles
//! answer, not for asking forever — so [`spawn`] retries every
//! [`RETRY_MS`] until one pass gets an answer from each, and then the thread
//! exits and the feature costs nothing at all for the life of the process.
//!
//! Stopping loses nothing, which is the test that makes it the right shape: after
//! one answered pass every live session either has a row or was skipped for a
//! reason no amount of waiting changes — no title we recognize (and a title
//! appears only when the dashboard writes one, which needs a row), or a session
//! this dashboard has never seen. A gate that only ever re-asks the same question
//! is a poll with extra steps, and it is not free: `gap_exists` is cheap, but it
//! sits downstream of `SessionRegistry::live_sessions`, whose 5s cache is always
//! stale at any sane tick — so every wake paid a directory read and a full
//! process enumeration to rediscover the same nothing.
//!
//! Consequence, and it matches how this app treats its other start-only settings
//! (`sync.listen`, `listen_port`, `bind_scope`): turning `restore_sessions` or
//! `terminal_titles` on at runtime takes effect at the next start, not
//! immediately.

use tauri::{AppHandle, Manager};

use crate::commands::now_ms;
use crate::config::ConfigState;
use crate::session_registry::{Activity, LiveSession, SessionRegistry};
use crate::state::{AppState, SetInput, Status};
use crate::terminals::TerminalSession;

/// How long to wait between attempts while the terminal or the registry is still
/// coming up.
///
/// Short, because this is startup latency the user is watching — the widget is
/// blank until the first answered pass — and because it is paid at most
/// [`MAX_ATTEMPTS`] times, not forever.
const RETRY_MS: u64 = 3_000;

/// How many unanswered attempts before giving up for this run.
///
/// A bound rather than a deadline, and it has to exist: on a machine where the
/// terminal is simply never running, "retry until it answers" is the same
/// infinite poll this design removed. ~1 minute is far longer than a login takes
/// and still finite. Giving up is safe — it restores the behaviour this feature
/// replaced, where a session appears on its next hook event.
const MAX_ATTEMPTS: u32 = 20;

/// One session that should have a row and does not, with everything needed to
/// create it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restorable {
    /// The row id — the anchored id where `ChatIdRegistry` has one, so a session
    /// that has `cd`-ed rejoins its own row instead of forking a second one under
    /// its current directory's derivation.
    pub id: String,
    pub status: Status,
    /// When that status began, from the registry's own `statusUpdatedAt`.
    pub state_entered_at: i64,
    /// The owning process, or `None` where the registry collapsed more than one
    /// session into this row and naming an owner would be a guess.
    pub pid: Option<u32>,
}

/// Whether any live session lacks a row — the whole gate on spending a
/// subprocess.
///
/// Pure, so the steady-state cost is pinned by a test rather than measured. The
/// common case is every live session already having a row, which is what the
/// machine looks like within moments of a restart and for the rest of the day.
pub fn gap_exists(live: &[LiveSession], rows: &[String], anchored: &dyn Fn(&str) -> Option<String>) -> bool {
    live.iter().filter(|s| s.known_to(anchored)).any(|s| !rows.iter().any(|id| *id == s.row_id(anchored)))
}

/// The status to restore, given what the tab says and what the registry sees.
///
/// `Idle`, `Blocked`, `Done` and `Error` are settled states: they describe a turn
/// that has already ended, they do not expire while the dashboard is down, and
/// the registry cannot express them at all. The title is the only witness and it
/// is taken at its word.
///
/// `Working` and `Waiting` are claims about *now*. The glyph for either is as
/// stale as the downtime, and every way of being wrong about them is expensive —
/// `lid_awake` holds off system sleep for both, the row reads in-flight forever
/// on this dashboard and on every synced peer, and a `Waiting` restored without
/// `waiting_backstop_armed` (which no longer exists after a restart) can never be
/// settled by its backstop. So they survive only where the registry independently
/// reports a turn running, and otherwise degrade to `Idle` — which asserts the one
/// thing still known to be true, that the session is alive.
///
/// The degrade is deliberately not to `Done`: that would claim a finished turn
/// the user has not seen, raising an attention flag out of an absence of
/// evidence. Every source in this module is a positive observation.
pub fn restored_status(from_title: Status, activity: Activity) -> Status {
    match from_title {
        Status::Working | Status::Waiting if activity != Activity::Busy => Status::Idle,
        s => s,
    }
}

/// Match each live session to the tab showing it, and decide what to restore.
///
/// Pure: the whole judgment, testable without a terminal, a registry or an
/// `AppHandle`. `derive` is the caller's `derive_chat_id` closure (it needs
/// `projects_root`), and `anchored` its `ChatIdRegistry` lookup.
///
/// A live session is skipped, rather than restored with a fallback, when its tab
/// carries no title this dashboard recognizes — a tab we blanked when the row was
/// removed, one running something other than an agent, one from before this
/// dashboard ever ran. Skipping is what keeps the removal paths and this one from
/// fighting: `terminal_title::sync` blanks the tab of a row that goes away, so the
/// blank is the record that it went.
///
/// Two tabs deriving one session are only trusted when they agree. They normally
/// do — a split shows the same session twice — but a genuine disagreement means
/// two different conversations under one row id, and picking one of two statuses
/// would be arbitrary in the direction that shows work as attended-to.
pub fn plan(
    live: &[LiveSession],
    tabs: &[TerminalSession],
    rows: &[String],
    now_ms: i64,
    derive: &dyn Fn(&str) -> String,
    anchored: &dyn Fn(&str) -> Option<String>,
) -> Vec<Restorable> {
    let mut out = Vec::new();
    for s in live {
        // A session this dashboard has never processed an event for is one whose
        // tab it never titled, so the glyph sitting there describes some earlier
        // conversation. See `LiveSession::known_to`.
        if !s.known_to(anchored) {
            continue;
        }
        let id = s.row_id(anchored);
        if rows.iter().any(|r| *r == id) {
            continue;
        }
        let Some(from_title) = title_status(&s.chat_id, tabs, derive) else { continue };
        out.push(Restorable {
            id,
            status: restored_status(from_title, s.activity),
            // The registry stamps `statusUpdatedAt` when a turn settles, which is
            // the instant the status began. `now` would be a fabricated age, and
            // age is load-bearing in three places at once: the widget's elapsed
            // clock, `/api/agents`' `status_age_ms` — whose entire purpose is
            // letting a caller weigh how old a reading is — and the notifier's
            // time-in-state.
            state_entered_at: s.activity_age_ms.map_or(now_ms, |age| now_ms - age),
            // Ambiguity is refused rather than resolved: see the module note on
            // why the speaker's pid is not the row's owner when two collapse.
            pid: (s.sessions == 1).then_some(s.pid),
        });
    }
    out
}

/// The status the tabs showing `chat_id` agree on, or `None`.
fn title_status(chat_id: &str, tabs: &[TerminalSession], derive: &dyn Fn(&str) -> String) -> Option<Status> {
    let mut found: Option<Status> = None;
    for tab in tabs {
        let Some(cwd) = tab.cwd.as_deref() else { continue };
        if derive(cwd) != chat_id {
            continue;
        }
        let Some(reading) = tab.title.as_deref().and_then(crate::terminal_title::parse_title) else { continue };
        match found {
            Some(prev) if prev != reading.status => return None,
            _ => found = Some(reading.status),
        }
    }
    found
}

/// What one pass concluded, and therefore whether to run another.
///
/// The distinction is only ever *did both oracles answer*, never *did anything
/// get restored*: a pass that answers "nothing here is restorable" is a complete
/// answer and retrying it would ask the same question forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pass {
    /// Both sources answered. This run is over, whatever the answer was.
    Answered,
    /// The terminal or the registry could not be asked — the login race, most
    /// likely, since both start alongside the dashboard in no fixed order.
    Unanswered,
    /// The feature is off, or its state is not up yet. Nothing to wait for.
    Skip,
}

/// Restore what can be restored, retrying only while something has yet to answer.
///
/// A no-op where no terminal adapter exists — Windows today, which means a row
/// there stays absent until its session next acts. That is the honest gap rather
/// than a guess: the registry alone would say a session is alive without being
/// able to say anything about what it is doing, and `Idle` for a blocked agent is
/// a wrong claim, not a missing one.
///
/// Its own adapter instance rather than sharing `attention`'s: listing tabs holds
/// no state, the two run on different threads, and sharing a
/// `&mut dyn TerminalAdapter` between them would buy a lock for nothing.
pub fn spawn(app: AppHandle) {
    let Some(adapter) = crate::terminals::for_platform(&app) else {
        tracing::info!("no terminal adapter on this platform; sessions are not restored after a restart");
        return;
    };
    std::thread::spawn(move || {
        for attempt in 1..=MAX_ATTEMPTS {
            match tick(&app, adapter.as_ref()) {
                Pass::Answered => return,
                Pass::Skip if attempt == 1 => return, // the feature is off; nothing will change it before the next start
                _ => std::thread::sleep(std::time::Duration::from_millis(RETRY_MS)),
            }
        }
        // Reported rather than dropped: a machine whose terminal never answered
        // still shows every session on its next hook event, but the *reason* the
        // widget came up blank is not otherwise recoverable from the log.
        tracing::warn!(
            terminal = adapter.name(),
            attempts = MAX_ATTEMPTS,
            "gave up restoring sessions; the terminal or the session registry never answered. Rows will appear as each session next acts"
        );
    });
}

/// One pass.
fn tick(app: &AppHandle, adapter: &dyn crate::terminals::TerminalAdapter) -> Pass {
    let terminal = adapter.name();
    let Some(cfg_state) = app.try_state::<ConfigState>() else { return Pass::Unanswered };
    let cfg = cfg_state.snapshot();
    // Both halves of the gate, because the second is not merely a related
    // setting: the tab title is where a row's status survives this process, so
    // with titling off nothing has been writing them — and, worse, nothing is
    // *blanking* them either, since `terminal_title::sync` returns before its
    // removal sweep. The glyphs then freeze at whatever was last written while it
    // was on, and restoring from them would resurrect a status that is arbitrarily
    // old and a row that may have been deliberately removed.
    if !cfg.restore_sessions || !cfg.terminal_titles {
        return Pass::Skip;
    }
    let (Some(state), Some(registry)) = (app.try_state::<AppState>(), app.try_state::<SessionRegistry>()) else { return Pass::Unanswered };
    let now = now_ms();
    let root = cfg.projects_root.clone();
    let anchor_store = app.try_state::<crate::chat_id_registry::ChatIdRegistry>();
    let anchored = |sid: &str| anchor_store.as_ref().and_then(|r| r.anchored(sid));
    let derive = |cwd: &str| crate::adapters::claude::derive_chat_id(Some(cwd), root.as_deref());

    // Unreadable is not empty. `None` here means the registry directory could not
    // be read, which says nothing about what is running; treating it as "nothing
    // is alive" would be a check that never ran reading as one that passed.
    let Some(live) = registry.live_sessions(root.as_deref(), now) else {
        tracing::debug!(decision = "restore_scan", terminal, outcome = "registry_unreadable", "the session registry could not be read");
        return Pass::Unanswered;
    };
    let rows: Vec<String> = state.snapshot().into_iter().map(|s| s.id).collect();
    if !gap_exists(&live, &rows, &anchored) {
        // A complete answer, not a deferral: every live session this dashboard
        // knows already has a row, so there is nothing a later attempt could add.
        tracing::debug!(decision = "restore_scan", terminal, outcome = "no_gap", live = live.len(), rows = rows.len(), "every live session already has a row");
        return Pass::Answered;
    }
    let Some(tabs) = adapter.sessions() else {
        tracing::debug!(decision = "restore_scan", terminal, outcome = "no_answer", live = live.len(), rows = rows.len(), "the terminal could not be asked what it is showing");
        return Pass::Unanswered;
    };

    let plan = plan(&live, &tabs, &rows, now, &derive, &anchored);
    let history = app.try_state::<crate::prompt_history::PromptHistoryStore>();
    let mut restored = 0;
    for r in &plan {
        let input = SetInput {
            id: r.id.clone(),
            status: r.status,
            // No label: one was never observed, and `label_policy` has nothing to
            // select from. The frontend already falls back to the last task in the
            // restored dialog, which is a thing that was actually said.
            label: None,
            source: None,
            model: None,
            input_tokens: None,
            dialog_entry: None,
            waiting_backstop_armed: false,
        };
        // Whatever was persisted for this row: its dialog, its originating prompt
        // and its task clock. Nothing here is a guess — it is what the row held
        // when it was last written out, and `apply_set`'s separator rule applies
        // to it exactly as it does on the hook path.
        let persisted = history.as_ref().and_then(|h| h.get(&r.id));
        let dialog_entries = persisted.as_ref().map_or(0, |p| p.dialog.len());
        if !state.restore_row(input, r.state_entered_at, now, persisted) {
            continue; // a hook event created it between the snapshot and here
        }
        if let Some(pid) = r.pid {
            // Give the reaper the owning pid it would otherwise never learn for
            // this row. Without it a restored row is the one thing the reaper
            // exists to prevent: a session that exits without a `SessionEnd`,
            // stranded forever — and worse than the hook case, because a restored
            // row also has no owner in `ChatIdRegistry`, so nothing removes it.
            if let Some(pids) = app.try_state::<crate::liveness::AgentPids>() {
                pids.set(&r.id, pid);
            }
            // And give `terminal_title` a console candidate, so the blank it
            // writes when a row goes away can still reach the tab. Its own
            // resolver prefers the registry and only falls back to this chain,
            // which is exactly the case that matters here: once the process is
            // gone the registry has no answer, and without a candidate the tab
            // would keep the last glyph this dashboard wrote — the stale-title
            // state this whole module exists to read, left behind by it.
            if let Some(titles) = app.try_state::<crate::terminal_title::TerminalTitles>() {
                titles.register(&r.id, &[pid]);
            }
        }
        restored += 1;
        tracing::debug!(
            chat_id = %r.id,
            decision = "restore_row",
            terminal,
            status = ?r.status,
            age_ms = now - r.state_entered_at,
            pid = ?r.pid,
            dialog_entries,
            reason = "a live session with no row; status read back from the tab title this dashboard last wrote",
            "decision"
        );
    }
    // Logged on every pass that got as far as asking, including the ones
    // restoring nothing: a sensor whose success and whose total failure are both
    // silent cannot be told apart from one that never ran.
    tracing::debug!(
        decision = "restore_scan",
        terminal,
        outcome = if restored > 0 { "restored" } else { "nothing_restorable" },
        live = live.len(),
        rows = rows.len(),
        tabs = tabs.len(),
        planned = plan.len(),
        restored,
        "session restore pass"
    );
    if restored > 0 {
        crate::commands::emit_sessions_updated(app);
    }
    // Both sources answered, so this run is over even when nothing was
    // restorable: the reasons a session is skipped — no title we recognize, never
    // seen by this dashboard — are not ones another attempt would change.
    Pass::Answered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(chat_id: &str, activity: Activity, age_ms: Option<i64>, sessions: usize) -> LiveSession {
        LiveSession {
            chat_id: chat_id.into(),
            name: Some(chat_id.into()),
            activity,
            activity_age_ms: age_ms,
            sessions,
            session_ids: vec![format!("sid-{chat_id}")],
            pid: 4_242,
        }
    }

    fn tab(cwd: &str, title: Option<&str>) -> TerminalSession {
        TerminalSession { cwd: Some(cwd.into()), title: title.map(str::to_string) }
    }

    fn derive(cwd: &str) -> String {
        cwd.rsplit('/').next().unwrap_or(cwd).to_string()
    }

    /// Every session in these tests is one this dashboard has seen — the ordinary
    /// case, and what `known_to` is checking. `sid-<chat_id>` matches the id
    /// [`live`] puts on each fixture.
    fn seen(sid: &str) -> Option<String> {
        sid.strip_prefix("sid-").map(str::to_string)
    }

    fn no_anchor(_: &str) -> Option<String> {
        None
    }

    fn planned(live: &[LiveSession], tabs: &[TerminalSession], rows: &[String]) -> Vec<Restorable> {
        plan(live, tabs, rows, 10_000, &derive, &seen)
    }

    #[test]
    fn a_live_session_with_no_row_is_restored_at_the_status_its_tab_holds() {
        // The motivating case: an agent parked on a question, invisible in the
        // widget because a row is only ever created by a hook event.
        let out = planned(&[live("what-is-next", Activity::Idle, Some(3_000), 1)], &[tab("/p/what-is-next", Some("✋ what-is-next"))], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "what-is-next");
        assert_eq!(out[0].status, Status::Blocked);
        assert_eq!(out[0].state_entered_at, 7_000, "the registry's own stamp, not now");
        assert_eq!(out[0].pid, Some(4_242));
    }

    #[test]
    fn a_session_that_already_has_a_row_is_left_alone() {
        let out = planned(&[live("dash", Activity::Busy, Some(10), 1)], &[tab("/p/dash", Some("🔵 dash"))], &["dash".to_string()]);
        assert!(out.is_empty());
    }

    #[test]
    fn a_tab_with_no_title_we_wrote_is_skipped_rather_than_guessed_at() {
        // This is also the anti-resurrection guard: `terminal_title::sync` blanks
        // the tab of a row that was removed, so the blank *is* the record that it
        // went, and a restore that fell back to a default status would undo every
        // `/clear` and every reap.
        for title in [None, Some(""), Some("~/p/dash — zsh"), Some("dash")] {
            assert!(planned(&[live("dash", Activity::Idle, Some(10), 1)], &[tab("/p/dash", title)], &[]).is_empty(), "{title:?}");
        }
    }

    #[test]
    fn a_live_session_with_no_tab_at_all_is_skipped() {
        // Verified live: one interactive session had no agterm tab. Nothing about
        // it is known beyond "alive", and `Idle` for an agent that may be blocked
        // is a wrong claim rather than a missing one.
        assert!(planned(&[live("headless", Activity::Idle, Some(10), 1)], &[], &[]).is_empty());
    }

    #[test]
    fn working_survives_only_while_the_registry_agrees_a_turn_is_running() {
        // The glyph is as stale as the downtime, which is unbounded. A phantom
        // `Working` row holds off system sleep via `lid_awake` and reads blue
        // forever here and on every synced peer.
        assert_eq!(restored_status(Status::Working, Activity::Busy), Status::Working);
        assert_eq!(restored_status(Status::Working, Activity::Idle), Status::Idle);
        assert_eq!(restored_status(Status::Working, Activity::Unknown), Status::Idle);
        assert_eq!(restored_status(Status::Waiting, Activity::Busy), Status::Waiting);
        assert_eq!(restored_status(Status::Waiting, Activity::Idle), Status::Idle);
    }

    #[test]
    fn a_settled_status_is_taken_from_the_tab_whatever_the_registry_says() {
        // `Activity` is idle/busy only and is deliberately never mapped onto a
        // `Status`: it cannot express any of these, so it gets no vote on them.
        for status in [Status::Idle, Status::Blocked, Status::Done, Status::Error] {
            for activity in [Activity::Idle, Activity::Busy, Activity::Unknown] {
                assert_eq!(restored_status(status, activity), status, "{status:?} / {activity:?}");
            }
        }
    }

    #[test]
    fn a_read_finished_row_comes_back_read_and_an_unread_one_comes_back_asking() {
        // The round-trip that makes `attended_at` not need restoring: titles are
        // written after `apply_read_as_idle`, so the flip is already in the glyph.
        let out = planned(&[live("agterm", Activity::Idle, Some(10), 1)], &[tab("/p/agterm", Some("🟢 agterm"))], &[]);
        assert_eq!(out[0].status, Status::Done, "unread");
        let out = planned(&[live("printlab", Activity::Idle, Some(10), 1)], &[tab("/p/printlab", Some("⚪ printlab"))], &[]);
        assert_eq!(out[0].status, Status::Idle, "read before the restart");
    }

    #[test]
    fn a_title_carrying_context_and_drift_suffixes_still_reads_its_status() {
        let out = planned(&[live("dash", Activity::Idle, Some(10), 1)], &[tab("/p/dash", Some("✋ dash [62%] ⚠"))], &[]);
        assert_eq!(out[0].status, Status::Blocked);
    }

    #[test]
    fn a_collapsed_row_is_restored_without_an_owning_pid() {
        // Two sessions in one directory (a `--fork-session --resume` migration).
        // The speaker is merely the freshest, so calling its pid the row's owner
        // would let the sibling's death reap a row the other still holds.
        let out = planned(&[live("what-is-next", Activity::Idle, Some(10), 2)], &[tab("/p/what-is-next", Some("✋ what-is-next"))], &[]);
        assert_eq!(out[0].pid, None);
    }

    #[test]
    fn two_tabs_on_one_session_are_trusted_only_when_they_agree() {
        let both = |a, b| planned(&[live("dash", Activity::Idle, Some(10), 1)], &[tab("/p/dash", Some(a)), tab("/p/dash", Some(b))], &[]);
        assert_eq!(both("🟢 dash", "🟢 dash")[0].status, Status::Done, "a split shows one session twice");
        assert!(both("🟢 dash", "✋ dash").is_empty(), "two statuses, and picking one would be arbitrary");
    }

    #[test]
    fn an_anchored_session_rejoins_its_own_row_rather_than_forking_a_second() {
        // A session that has `cd`-ed derives another row's id from its cwd. Under
        // the raw derivation this would create a row its next hook event declines
        // to use — an orphan with no owning pid and no owner.
        let anchored = |sid: &str| (sid == "sid-src-tauri").then(|| "dash".to_string());
        let out = plan(&[live("src-tauri", Activity::Idle, Some(10), 1)], &[tab("/p/src-tauri", Some("🟢 dash"))], &[], 10_000, &derive, &anchored);
        assert_eq!(out[0].id, "dash");
        let none = plan(&[live("src-tauri", Activity::Idle, Some(10), 1)], &[tab("/p/src-tauri", Some("🟢 dash"))], &["dash".to_string()], 10_000, &derive, &anchored);
        assert!(none.is_empty(), "and it is then seen to already have one");
    }

    #[test]
    fn a_session_with_no_status_stamp_is_credited_to_now() {
        // Not backdated to the epoch, which would read as an infinitely old
        // status and trip every age-based reader at once.
        let out = planned(&[live("dash", Activity::Idle, None, 1)], &[tab("/p/dash", Some("🟢 dash"))], &[]);
        assert_eq!(out[0].state_entered_at, 10_000);
    }

    #[test]
    fn nothing_is_asked_of_the_terminal_while_every_live_session_has_a_row() {
        // The steady state, and what the machine looks like for most of the day.
        let live = vec![live("a", Activity::Idle, Some(10), 1), live("b", Activity::Busy, Some(10), 1)];
        assert!(!gap_exists(&live, &["a".to_string(), "b".to_string()], &seen));
        assert!(gap_exists(&live, &["a".to_string()], &seen));
        assert!(!gap_exists(&[], &[], &seen), "nothing running");
        assert!(!gap_exists(&[], &["stale".to_string()], &seen), "a row with no live session is not this module's business");
    }

    #[test]
    fn only_an_unanswered_pass_is_worth_retrying() {
        // The rule the whole shape rests on: retry while something has yet to
        // answer, stop once both have. `Answered` must not be conditioned on
        // anything having been *restored*, or a machine with a permanently
        // unrestorable session would retry forever — which is the poll this
        // replaced, wearing a bound.
        assert_eq!(Pass::Answered, Pass::Answered);
        assert_ne!(Pass::Answered, Pass::Unanswered);
        assert_ne!(Pass::Skip, Pass::Unanswered);
    }

    #[test]
    fn nothing_restorable_is_still_a_complete_answer() {
        // Both sources answered and the plan is empty, because the only live
        // session has no title we wrote. Waiting cannot change that: a title
        // appears only when the dashboard writes one, and writing one needs a row.
        let live = [live("dash", Activity::Idle, Some(10), 1)];
        assert!(planned(&live, &[tab("/p/dash", None)], &[]).is_empty());
        // And the gap is still open, so a design that retried "until the gap
        // closes" rather than "until both answered" would never stop here.
        assert!(gap_exists(&live, &[], &seen));
    }

    #[test]
    fn a_session_this_dashboard_has_never_seen_is_neither_restored_nor_polled_for() {
        // Nothing rewrites a tab title while the dashboard is down, so a tab can
        // carry a `✋` from a conversation that ended days ago while a brand-new
        // session runs in it. Restoring that glyph would assert a question nobody
        // asked — and it is the *only* case where the tab's status and the live
        // session are unrelated, because a session we have seen is one we titled
        // that tab for.
        let live = [live("dash", Activity::Idle, Some(10), 1)];
        let tabs = [tab("/p/dash", Some("✋ dash"))];
        assert!(plan(&live, &tabs, &[], 10_000, &derive, &no_anchor).is_empty());
        // And the gate agrees, so an unknown session does not keep the terminal
        // being asked every tick for something that will never be restored.
        assert!(!gap_exists(&live, &[], &no_anchor));
        // The same session, once seen, restores normally.
        assert_eq!(planned(&live, &tabs, &[])[0].status, Status::Blocked);
        assert!(gap_exists(&live, &[], &seen));
    }

    #[test]
    fn one_anchored_session_id_vouches_for_a_collapsed_row() {
        // Measured live: a fork migration left two sessions in one directory, one
        // of them never seen by this dashboard. The row is still ours.
        let only_second = |sid: &str| (sid == "sid-b").then(|| "what-is-next".to_string());
        let mut s = live("what-is-next", Activity::Idle, Some(10), 2);
        s.session_ids = vec!["sid-unknown".into(), "sid-b".into()];
        assert!(s.known_to(&only_second));
        assert_eq!(plan(&[s], &[tab("/p/what-is-next", Some("✋ what-is-next"))], &[], 10_000, &derive, &only_second).len(), 1);
    }

    #[test]
    fn the_gap_check_resolves_anchors_too() {
        // Otherwise a `cd`-ed session with a perfectly good row would look missing
        // on every tick, and the terminal would be asked forever.
        let anchored = |sid: &str| (sid == "sid-src-tauri").then(|| "dash".to_string());
        assert!(!gap_exists(&[live("src-tauri", Activity::Idle, Some(10), 1)], &["dash".to_string()], &anchored));
    }
}
