//! Mirroring each row's current prompt onto a terminal's own per-session
//! context line.
//!
//! Some terminals give a session a field the console title never reaches, and
//! two of them do: agwinterm on Windows draws a dim context line beside each
//! session's name, in the sidebar and in the title bar, and agterm on macOS
//! shows one in the title bar of its window's active session. The name needs no
//! writing from here, because each terminal's own label follows the program
//! title of the session's focused pane, which for an agent is the console title
//! `terminal_title::sync` already writes, glyph and suffix included. So the one
//! field written is the context, and it carries the row's task
//! (`AgentSession::shown_task`), what the session was asked to do. The status is
//! not repeated there: the title's glyph already says it.
//!
//! **The budget is the one place the two terminals genuinely differ**, and it is
//! not a difference of degree: `LabelTarget::budget` names its unit, because
//! agwinterm's 200 UTF-16 units are a display cap that shows a longer write cut,
//! while agterm's 256 UTF-8 bytes are enforced and an over-long write is refused
//! with the previous context left standing. Since [`pass`] abandons the rest of
//! its pass on the first `Err`, a row fitted in the wrong unit would starve
//! every row planned after it for as long as the row lived. [`fit`] dispatches on
//! the unit the terminal reported.
//!
//! **A session is joined to a row by its title.** The title is the string this
//! dashboard wrote, so it names the row rather than guessing at it, and it is
//! read by the rule `crate::attention` names a session by
//! ([`crate::terminal_title::title_names`]): the longest label wins, and a draw
//! between identically labelled rows is refused. A row whose title two sessions
//! carry is refused too. A session whose focused pane is not the agent, a shell
//! beside it in a split for one, carries that pane's title and is not joined.
//!
//! **The context of a session this dashboard labels is its own.** It is
//! overwritten or cleared whatever it held, and the log keeps the value replaced.
//! A session no row claims has its context cleared only where it is ours: its
//! title is one of this dashboard's titles naming no live row, or a draw, or a
//! row two sessions carry; or its context still reads exactly what this process
//! wrote there for a row that has since left, which is what reaches a session
//! whose title went blank when its row did, and every session when
//! `terminal_titles` is turned off. A context on a session whose title is not ours
//! and which this process did not write is never touched. The record of what was
//! written is in memory only, so a context a previous process wrote on a session
//! whose title has since stopped being ours stays until the user clears it.
//!
//! **Nothing here names a terminal.** Whether a terminal has a context line at
//! all is [`super::TerminalAdapter::can_label`]; what each session shows now
//! comes from [`super::TerminalAdapter::label_targets`]; a write goes through
//! [`super::TerminalAdapter::write_label`]. This module owns when to ask and the
//! pure [`plan`] that turns the rows and the live reading into writes.
//!
//! **`sync` hands over and never waits.** It runs on the emitting thread, which
//! can be the hook handler or the Tauri main thread, and a write is a
//! cross-process call that can take seconds. So [`request`] only replaces a
//! latest-wins slot, and a worker thread does the reading and writing.
//!
//! **Every write is decided against the terminal's live reading.** That is what
//! catches the user changing a context, and the terminal losing one.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::AppHandle;

use super::{LabelBudget, LabelTarget, LabelWrite, TerminalAdapter};
use crate::terminal_title::{title_names, Named};

/// How long requests have to stop arriving before the worker acts, so a `/clear`
/// that removes a row and recreates it in the next emit is never seen in between.
/// Counted from the latest request, not the first, and a request that removed a
/// row always gets the whole of it, [`DEBOUNCE_CAP`] notwithstanding.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// The longest the worker waits for requests to stop, so a steady stream of
/// emits cannot hold every context back. It does not cut short the debounce
/// after a removal: that is what keeps a `/clear` arriving late in a busy stretch
/// from being read between its removal and its recreation, and a removal can only
/// recur once its row has come back.
const DEBOUNCE_CAP: Duration = Duration::from_millis(1_000);

/// How long an unchanged set goes without a fresh reading. Without the wait,
/// every emit the transcript watcher makes during a turn would cost a `tree`
/// read; with it, a session that took a row's title, a context changed in the
/// terminal and a terminal restart are all corrected within this long.
const RESYNC: Duration = Duration::from_millis(15_000);

/// How long after a pass that could not look, or whose write failed, the next one
/// runs.
const RETRY: Duration = Duration::from_millis(10_000);

/// How often, and how many times, the worker asks whether its terminal labels at
/// all before giving up for this run.
///
/// The same shape, and the same reason, as `session_restore`'s retry: the
/// dashboard and the terminal both start at login in no fixed order, so the
/// first ask routinely reaches a terminal that is not up yet. Bounded so a
/// terminal that never answers is reported once instead of polled forever.
const CAN_LABEL_RETRY: Duration = Duration::from_millis(3_000);
const CAN_LABEL_ATTEMPTS: u32 = 20;

/// `text` as one line: every run of whitespace and control characters becomes
/// one space and the ends are trimmed. `None` when nothing is left. Not yet
/// fitted to any terminal's budget, which [`fit_utf16`] does per target.
///
/// HTML folds only ASCII whitespace, while this folds Rust's whole `White_Space`
/// set, so a run of no-break or em spaces the widget row draws as is becomes one
/// space here.
pub fn one_line(text: &str) -> Option<String> {
    let line = text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// `line` within `max` UTF-16 code units: whole where it fits, else cut on a
/// character boundary and ended with `…` inside the budget, with no space left
/// before the ellipsis. A budget of at least one unit is assumed, the ellipsis
/// being one.
pub fn fit_utf16(line: &str, max: usize) -> String {
    if line.encode_utf16().count() <= max {
        return line.to_string();
    }
    // One unit is kept back for the ellipsis.
    let mut units = 0;
    let cut = line.char_indices().find(|(_, c)| {
        units += c.len_utf16();
        units > max.saturating_sub(1)
    });
    let head = cut.map_or(line, |(at, _)| &line[..at]);
    format!("{}…", head.trim_end())
}

/// `line` within `max` UTF-8 bytes, the same shape as [`fit_utf16`] in the unit
/// agterm enforces. The ellipsis is three bytes, so that much is kept back.
///
/// The `trim_end` is not tidiness: agterm stores the *trimmed* value, so a cut
/// landing after a space would make the read-back differ from what was sent and
/// [`plan`] would rewrite the same text on every pass, forever.
pub fn fit_utf8(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.trim_end().to_string();
    }
    const ELLIPSIS: usize = '…'.len_utf8();
    let room = max.saturating_sub(ELLIPSIS);
    let mut head = &line[..0];
    for (at, c) in line.char_indices() {
        if at + c.len_utf8() > room {
            break;
        }
        head = &line[..at + c.len_utf8()];
    }
    format!("{}…", head.trim_end())
}

/// `line` within `target`'s budget, in whichever unit that terminal enforces.
pub fn fit(line: &str, budget: LabelBudget) -> String {
    match budget {
        LabelBudget::Utf16(max) => fit_utf16(line, max),
        LabelBudget::Utf8Bytes(max) => fit_utf8(line, max),
    }
}

/// One row, as `terminal_title::sync` wants its sessions labelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredLabel {
    pub row: String,
    /// What the row's tab title carries after its glyph,
    /// `AgentSession::display_label`, which a session's title is matched against.
    pub label: String,
    /// The [`one_line`] of the row's task, or `None` when the row has none, which
    /// clears the context. Fitted to each target's budget by [`plan`].
    pub context: Option<String>,
}

/// The rows to label, from one snapshot.
#[derive(Clone, Debug)]
struct LabelSet {
    seq: u64,
    rows: Vec<DesiredLabel>,
    /// When it was handed over, which is what the debounce counts from.
    at: Instant,
}

#[derive(Default)]
struct Slot {
    /// `None` until the first request: an unknown set is not an empty one, and an
    /// empty one withdraws every context of ours.
    set: Option<LabelSet>,
    /// Whether a request arrived since the worker last took the set.
    fresh: bool,
    /// When the latest request arrived that dropped a row the set before it had.
    removed_at: Option<Instant>,
}

static SLOT: OnceLock<(Mutex<Slot>, Condvar)> = OnceLock::new();
static WORKER: OnceLock<()> = OnceLock::new();

fn slot() -> &'static (Mutex<Slot>, Condvar) {
    SLOT.get_or_init(Default::default)
}

/// Whether a set from snapshot `seq` replaces the one held. `sync` already drops
/// a stale snapshot before it gets here, so this only keeps the slot from going
/// backwards if two hand-offs ever crossed.
fn newer(held: Option<u64>, seq: u64) -> bool {
    held.is_none_or(|h| seq > h)
}

/// Hand the worker the labels snapshot `seq` wants. Never does I/O and never
/// waits on the worker.
pub fn request(seq: u64, rows: Vec<DesiredLabel>) {
    let (lock, cv) = slot();
    let mut held = lock.lock().unwrap();
    if !newer(held.set.as_ref().map(|s| s.seq), seq) {
        return;
    }
    let now = Instant::now();
    if held.set.as_ref().is_some_and(|s| drops_a_row(&s.rows, &rows)) {
        held.removed_at = Some(now);
    }
    held.set = Some(LabelSet { seq, rows, at: now });
    held.fresh = true;
    cv.notify_one();
}

/// Whether `next` lacks a row `prev` had.
fn drops_a_row(prev: &[DesiredLabel], next: &[DesiredLabel]) -> bool {
    prev.iter().any(|p| !next.iter().any(|n| n.row == p.row))
}

/// How much longer to wait for requests to stop arriving, or `None` once they
/// have: [`DEBOUNCE`] after the latest one, and never past [`DEBOUNCE_CAP`]
/// after the worker began waiting unless a removal still has part of its own
/// debounce to run.
fn settle_left(began: Instant, latest: Instant, removed_at: Option<Instant>, now: Instant) -> Option<Duration> {
    let cap = removed_at.map_or(began + DEBOUNCE_CAP, |r| (r + DEBOUNCE).max(began + DEBOUNCE_CAP));
    let until = (latest + DEBOUNCE).min(cap);
    (now < until).then(|| until - now)
}

/// When the previous pass ran, and whether it could not look or failed a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LastPass {
    at: Instant,
    failed: bool,
}

/// When the next pass is due with nothing new to label.
fn due_at(last: LastPass) -> Instant {
    last.at + if last.failed { RETRY } else { RESYNC }
}

/// Whether to read the terminal now. A changed set always reads; an unchanged one
/// waits out the resync, or the retry after a failed pass.
fn should_read(changed: bool, last: Option<LastPass>, now: Instant) -> bool {
    changed || last.is_none_or(|l| now >= due_at(l))
}

/// Start the worker, if this platform has a terminal with a context line. A
/// no-op if called twice.
///
/// Two ways it declines to run: `for_platform` answering `None` (no terminal
/// adapter at all), and an adapter whose terminal has no context line. The second
/// is asked of [`super::TerminalAdapter::can_label`] rather than learned from
/// `label_targets`, whose `None` also means "could not look just now" and is
/// retried, so a terminal with no context line would otherwise be asked every few
/// seconds for the life of the process.
///
/// Not gated on `terminal_titles`, which hot-reloads: with titles off, `sync`
/// hands over an empty set, and that withdraws every context of ours.
pub fn spawn(app: AppHandle) {
    let Some(adapter) = super::for_platform(&app) else { return };
    if WORKER.set(()).is_err() {
        return;
    }
    // The capability question is asked on the WORKER thread, never here. This
    // runs inside Tauri's `setup` at `RunEvent::Ready`, i.e. on the main thread,
    // and agterm's answer costs a subprocess that at login may have to be
    // retried for a minute — a wait there freezes the widget, the tray and the
    // history window, which is the deadlock `commands::emit_sessions_updated`
    // documents and which was already shipped and reverted once.
    std::thread::spawn(move || {
        let adapter = adapter.as_ref();
        let terminal = adapter.name();
        for attempt in 1..=CAN_LABEL_ATTEMPTS {
            match adapter.can_label() {
                Some(true) => return run(adapter),
                Some(false) => {
                    tracing::debug!(terminal, "this terminal has no session context line; the label worker does not start");
                    return;
                }
                None => {
                    tracing::debug!(terminal, attempt, "could not ask this terminal whether it labels; retrying");
                    std::thread::sleep(CAN_LABEL_RETRY);
                }
            }
        }
        // Reported rather than retried forever: a terminal that never answers is
        // a fact worth seeing in the log, and the dashboard and the terminal both
        // start at login, so the window this covers is seconds, not hours.
        tracing::warn!(terminal, attempts = CAN_LABEL_ATTEMPTS, "no answer on whether this terminal labels; giving up for this run");
    });
}

fn run(adapter: &dyn TerminalAdapter) {
    let terminal = adapter.name();
    let (lock, cv) = slot();
    let mut last: Option<LastPass> = None;
    let mut read_for: Option<Vec<DesiredLabel>> = None;
    let mut seen = Seen::default();
    let mut written: HashMap<String, Placed> = HashMap::new();
    let mut last_outcome: Option<&'static str> = None;
    loop {
        let set = {
            let mut held = lock.lock().unwrap();
            while held.set.is_none() {
                held = cv.wait(held).unwrap();
            }
            if !held.fresh {
                let wait = last.map_or(Duration::ZERO, |l| due_at(l).saturating_duration_since(Instant::now())).max(Duration::from_millis(1));
                held = cv.wait_timeout(held, wait).unwrap().0;
            }
            if held.fresh {
                let began = Instant::now();
                let latest = |h: &Slot| h.set.as_ref().map_or(began, |s| s.at);
                while let Some(left) = settle_left(began, latest(&held), held.removed_at, Instant::now()) {
                    held = cv.wait_timeout(held, left).unwrap().0;
                }
            }
            held.fresh = false;
            held.set.clone().expect("the wait above returns only once a set is held")
        };
        let changed = read_for.as_ref() != Some(&set.rows);
        if !should_read(changed, last, Instant::now()) {
            continue;
        }
        let outcome = pass(adapter, &set.rows, &mut seen, &mut written);
        last = Some(LastPass { at: Instant::now(), failed: matches!(outcome, Outcome::Unanswered | Outcome::WriteFailed) });
        read_for = Some(set.rows);
        let slug = outcome.slug();
        // Every pass at debug, and at info whenever the outcome moves, so a
        // terminal that never answers says so once rather than every few seconds
        // and a steady state is quiet without being silent.
        if last_outcome != Some(slug) {
            tracing::info!(decision = "label_pass", terminal, outcome = slug, "terminal labels pass");
            last_outcome = Some(slug);
        } else {
            tracing::debug!(decision = "label_pass", terminal, outcome = slug, "terminal labels pass");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Something was written.
    Applied,
    /// Nothing was written: every context already read as wanted.
    Unchanged,
    /// The terminal could not be asked.
    Unanswered,
    /// A write was refused or lost; the rest of the pass was abandoned.
    WriteFailed,
}

impl Outcome {
    fn slug(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::Unchanged => "unchanged",
            Outcome::Unanswered => "unanswered",
            Outcome::WriteFailed => "write_failed",
        }
    }
}

/// What the previous pass reported, so each line marks a change rather than
/// repeating every pass.
#[derive(Default)]
struct Seen {
    declined: HashMap<String, (&'static str, usize)>,
}

/// One read and the writes it calls for. `written` is the context this worker
/// last wrote to each target and has not cleared since, kept up to date here.
fn pass(adapter: &dyn TerminalAdapter, rows: &[DesiredLabel], seen: &mut Seen, written: &mut HashMap<String, Placed>) -> Outcome {
    let Some(targets) = adapter.label_targets() else { return Outcome::Unanswered };
    let plan = plan(rows, &targets, written);
    written.extend(plan.in_place.iter().cloned());
    // Edge-only: a row is reported when it starts being declined or its reason
    // changes, not on every pass it stays that way.
    for d in plan.declined.iter().filter(|d| seen.declined.get(&d.row) != Some(&(d.reason, d.n))) {
        tracing::info!(decision = "label_declined", chat_id = %d.row, reason = d.reason, n = d.n, "no session's context is labelled for this row, since its title does not name one session for it alone");
    }
    seen.declined = plan.declined.iter().map(|d| (d.row.clone(), (d.reason, d.n))).collect();
    let mut wrote = false;
    for w in &plan.writes {
        let started = Instant::now();
        let result = adapter.write_label(&w.key, &w.write);
        let ms = started.elapsed().as_millis() as u64;
        let value = match &w.write {
            LabelWrite::Context(v) => v.as_str(),
            LabelWrite::ClearContext => "",
        };
        let replaced = w.replaced.as_deref().unwrap_or("");
        match (&w.purpose, &result) {
            (Purpose::Label { row }, Ok(())) => tracing::info!(decision = "label_write", chat_id = %row, key = %w.key, value, replaced, ok = true, ms, "terminal context line written"),
            (Purpose::Label { row }, Err(error)) => tracing::warn!(decision = "label_write", chat_id = %row, key = %w.key, value, replaced, ok = false, ms, error = %error, "terminal context line write failed"),
            (Purpose::Withdraw, Ok(())) => tracing::info!(decision = "label_withdraw", key = %w.key, replaced, ok = true, ms, "a context line of ours that no row claims was cleared"),
            (Purpose::Withdraw, Err(error)) => tracing::warn!(decision = "label_withdraw", key = %w.key, replaced, ok = false, ms, error = %error, "clearing a context line of ours failed"),
        }
        // A failed write ends the pass. The next one re-reads the terminal and
        // plans again from what is really there, which costs less than pressing on
        // against a terminal that has just stopped answering.
        if result.is_err() {
            return Outcome::WriteFailed;
        }
        wrote = true;
        match (&w.write, &w.purpose) {
            (LabelWrite::Context(text), Purpose::Label { row }) => {
                written.insert(w.key.clone(), Placed { row: row.clone(), text: text.clone() });
            }
            _ => {
                written.remove(&w.key);
            }
        }
    }
    if wrote {
        Outcome::Applied
    } else {
        Outcome::Unchanged
    }
}

/// Why a write is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// To show this row's task.
    Label { row: String },
    /// To clear a context of ours that no row claims.
    Withdraw,
}

/// One write the plan calls for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedWrite {
    pub key: String,
    pub write: LabelWrite,
    /// The value the write replaces, for the log, so a context the user or an
    /// agent set stays visible after this overwrites it. `None` where the field
    /// was empty.
    pub replaced: Option<String>,
    pub purpose: Purpose,
}

/// What this process put on a session for a row, or found already there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub row: String,
    pub text: String,
}

/// A row no session is labelled for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
    pub row: String,
    /// `same_label` (a title names this row and others carrying the same label;
    /// `n` counts those rows) or `shared_title` (several sessions' titles name
    /// this row; `n` counts them).
    pub reason: &'static str,
    pub n: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// In the order to make them.
    pub writes: Vec<PlannedWrite>,
    pub declined: Vec<Declined>,
    /// Each labelled target already showing the context its row wants, keyed, so
    /// the worker counts it as one it wrote however it got there.
    pub in_place: Vec<(String, Placed)>,
}

/// What one session's title says about it.
enum Claim<'a> {
    /// The title is not one this dashboard wrote, or there is none.
    Foreign,
    /// One of this dashboard's titles, naming no row this session can carry.
    Ours,
    /// One of this dashboard's titles, naming this row alone.
    Row(&'a DesiredLabel),
}

/// The writes that bring every session's context in line with the rows. Pure:
/// the whole judgment, with every terminal fact already turned into a
/// [`LabelTarget`]. The rules are the module doc's; `written` is the record of
/// what this process placed, by key.
pub fn plan(rows: &[DesiredLabel], targets: &[LabelTarget], written: &HashMap<String, Placed>) -> Plan {
    let mut out = Plan::default();
    let mut claims: Vec<Claim> = Vec::with_capacity(targets.len());
    for t in targets {
        // Local rows only: a context line is written *into* a session, so it is
        // only ever about one this dashboard owns. A tab rendering another
        // machine's session reads as foreign here and is left alone, which is
        // also the only safe answer — the far machine owns that session's
        // context line as surely as it owns its title.
        let named = t.title.as_deref().and_then(|title| title_names(title, false, rows.iter().map(|r| (r, r.label.as_str()))));
        claims.push(match named {
            None => Claim::Foreign,
            Some(Named::One(r)) => Claim::Row(r),
            Some(Named::Nothing) => Claim::Ours,
            Some(Named::Drawn(drawn)) => {
                for r in &drawn {
                    if !out.declined.iter().any(|d| d.row == r.row) {
                        out.declined.push(Declined { row: r.row.clone(), reason: "same_label", n: drawn.len() });
                    }
                }
                Claim::Ours
            }
        });
    }
    for r in rows {
        let n = claims.iter().filter(|c| matches!(c, Claim::Row(x) if x.row == r.row)).count();
        if n > 1 {
            out.declined.push(Declined { row: r.row.clone(), reason: "shared_title", n });
            for c in claims.iter_mut().filter(|c| matches!(c, Claim::Row(x) if x.row == r.row)) {
                *c = Claim::Ours;
            }
        }
    }

    for (t, claim) in targets.iter().zip(&claims) {
        let ours = match claim {
            Claim::Row(r) => {
                let purpose = Purpose::Label { row: r.row.clone() };
                match (&t.context, r.context.as_deref().map(|c| fit(c, t.budget))) {
                    (Some(c), Some(want)) if *c == want => out.in_place.push((t.key.clone(), Placed { row: r.row.clone(), text: want })),
                    (shown, Some(want)) => out.writes.push(PlannedWrite { key: t.key.clone(), write: LabelWrite::Context(want), replaced: shown.clone(), purpose }),
                    (Some(c), None) => out.writes.push(PlannedWrite { key: t.key.clone(), write: LabelWrite::ClearContext, replaced: Some(c.clone()), purpose }),
                    (None, None) => {}
                }
                continue;
            }
            Claim::Ours => true,
            // Text this process wrote for a row that has since left.
            Claim::Foreign => written.get(&t.key).is_some_and(|p| t.context.as_ref() == Some(&p.text) && !rows.iter().any(|r| r.row == p.row)),
        };
        if let Some(c) = t.context.as_ref().filter(|_| ours) {
            out.writes.push(PlannedWrite { key: t.key.clone(), write: LabelWrite::ClearContext, replaced: Some(c.clone()), purpose: Purpose::Withdraw });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget the tests' terminal reports.
    const BUDGET: usize = 200;

    /// The row `id`, labelled `id` on its tab, wanting `context`.
    fn row(id: &str, context: Option<&str>) -> DesiredLabel {
        DesiredLabel { row: id.into(), label: id.into(), context: context.map(Into::into) }
    }

    /// The row `dash` wanting the context `Fix the build`.
    fn dash() -> DesiredLabel {
        row("dash", Some("Fix the build"))
    }

    /// A session titled `title`, showing `context`.
    fn target(key: &str, title: Option<&str>, context: Option<&str>) -> LabelTarget {
        LabelTarget { key: key.into(), title: title.map(Into::into), context: context.map(Into::into), budget: LabelBudget::Utf16(BUDGET) }
    }

    fn write(row: &str, key: &str, text: &str, replaced: Option<&str>) -> PlannedWrite {
        PlannedWrite { key: key.into(), write: LabelWrite::Context(text.into()), replaced: replaced.map(Into::into), purpose: Purpose::Label { row: row.into() } }
    }

    fn withdraw(key: &str, replaced: &str) -> PlannedWrite {
        PlannedWrite { key: key.into(), write: LabelWrite::ClearContext, replaced: Some(replaced.into()), purpose: Purpose::Withdraw }
    }

    fn fresh(rows: &[DesiredLabel], targets: &[LabelTarget]) -> Plan {
        plan(rows, targets, &HashMap::new())
    }

    fn placed(row: &str, text: &str) -> Placed {
        Placed { row: row.into(), text: text.into() }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_older_seq_never_replaces_a_newer_set() {
        assert!(newer(None, 1));
        assert!(newer(Some(4), 5));
        assert!(!newer(Some(5), 5));
        assert!(!newer(Some(5), 4));
    }

    #[test]
    fn an_unchanged_set_skips_the_read_until_resync() {
        let t0 = Instant::now();
        let last = Some(LastPass { at: t0, failed: false });
        assert!(should_read(false, None, t0), "the first pass always reads");
        assert!(!should_read(false, last, t0 + RESYNC - ms(1)));
        assert!(should_read(false, last, t0 + RESYNC));
        assert!(should_read(true, last, t0 + ms(1)), "a changed set reads at once");
    }

    #[test]
    fn a_failed_pass_retries_after_retry() {
        let t0 = Instant::now();
        let failed = Some(LastPass { at: t0, failed: true });
        assert!(!should_read(false, failed, t0 + RETRY - ms(1)));
        assert!(should_read(false, failed, t0 + RETRY));
        assert!(!should_read(false, Some(LastPass { at: t0, failed: false }), t0 + RETRY), "a pass that succeeded waits the full resync");
    }

    #[test]
    fn the_debounce_restarts_on_every_request() {
        let t0 = Instant::now();
        assert_eq!(settle_left(t0, t0, None, t0), Some(DEBOUNCE));
        assert_eq!(settle_left(t0, t0, None, t0 + DEBOUNCE), None, "quiet for the whole debounce");
        // A removal arriving late in the window opened by an earlier request gets
        // a full debounce of its own, so the recreation that follows it is seen.
        assert_eq!(settle_left(t0, t0 + ms(190), Some(t0 + ms(190)), t0 + ms(200)), Some(ms(190)));
        assert_eq!(settle_left(t0, t0 + ms(190), Some(t0 + ms(190)), t0 + ms(390)), None);
    }

    #[test]
    fn a_stream_of_requests_cannot_hold_the_labels_back_past_the_cap() {
        let t0 = Instant::now();
        assert_eq!(settle_left(t0, t0 + ms(950), None, t0 + ms(990)), Some(ms(10)));
        assert_eq!(settle_left(t0, t0 + ms(990), None, t0 + DEBOUNCE_CAP), None);
        // A removal long before the wait began extends nothing.
        assert_eq!(settle_left(t0, t0 + ms(990), Some(t0 - ms(5_000)), t0 + DEBOUNCE_CAP), None);
    }

    #[test]
    fn a_removal_late_in_a_busy_stretch_still_gets_its_whole_debounce() {
        // `/clear` removes the row 900ms into a stream of emits; the cap must not
        // read the set before the recreation 100ms later can arrive.
        let t0 = Instant::now();
        assert_eq!(settle_left(t0, t0 + ms(900), Some(t0 + ms(900)), t0 + DEBOUNCE_CAP), Some(ms(100)));
        assert_eq!(settle_left(t0, t0 + ms(1_000), Some(t0 + ms(900)), t0 + DEBOUNCE_CAP), Some(ms(100)), "the recreation does not restart the cap's extension");
        assert_eq!(settle_left(t0, t0 + ms(1_000), Some(t0 + ms(900)), t0 + ms(1_100)), None);
    }

    #[test]
    fn only_a_request_that_drops_a_row_counts_as_a_removal() {
        let a = row("a", None);
        let b = row("b", None);
        assert!(drops_a_row(&[a.clone(), b.clone()], std::slice::from_ref(&b)));
        assert!(!drops_a_row(std::slice::from_ref(&b), &[a.clone(), b.clone()]), "an addition is not a removal");
        assert!(!drops_a_row(&[a.clone(), b], &[a, row("b", Some("Fix the build"))]), "a changed context is not a removal");
    }

    #[test]
    fn a_session_titled_for_a_row_takes_its_task_over_whatever_it_held() {
        let p = fresh(&[dash()], &[target("k", Some("🔵 dash"), None)]);
        assert_eq!(p.writes, vec![write("dash", "k", "Fix the build", None)]);
        let p = fresh(&[dash()], &[target("k", Some("✋ dash [47%] ⚠"), Some("an agent's note"))]);
        assert_eq!(p.writes, vec![write("dash", "k", "Fix the build", Some("an agent's note"))], "the suffix does not stop the join, and the replaced text is kept for the log");
        assert!(p.declined.is_empty());
    }

    #[test]
    fn a_context_already_in_place_is_only_recorded() {
        let p = fresh(&[dash()], &[target("k", Some("🟢 dash"), Some("Fix the build"))]);
        assert_eq!(p, Plan { in_place: vec![("k".into(), placed("dash", "Fix the build"))], ..Plan::default() });
    }

    #[test]
    fn the_join_is_the_title_never_the_directory() {
        // A shell in the project directory, titled by its prompt, is not the
        // agent; nor is a session whose title is another row's.
        let p = fresh(&[dash()], &[target("shell", Some("~/p/dash — zsh"), None), target("web", Some("🔵 web"), None), target("none", None, None)]);
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn the_longest_label_a_title_names_wins() {
        let rows = [DesiredLabel { label: "bga".into(), ..row("bga", Some("Parent task")) }, DesiredLabel { label: "bga assistant".into(), ..row("bga/assistant", Some("Child task")) }];
        let p = fresh(&rows, &[target("k", Some("🟢 bga assistant [40%]"), None)]);
        assert_eq!(p.writes, vec![write("bga/assistant", "k", "Child task", None)]);
    }

    #[test]
    fn a_row_with_no_task_clears_its_sessions_context() {
        let p = fresh(&[row("dash", None)], &[target("k", Some("🔵 dash"), Some("Fix the build"))]);
        assert_eq!(p.writes, vec![PlannedWrite { key: "k".into(), write: LabelWrite::ClearContext, replaced: Some("Fix the build".into()), purpose: Purpose::Label { row: "dash".into() } }]);
        assert_eq!(fresh(&[row("dash", None)], &[target("k", Some("🔵 dash"), None)]), Plan::default(), "nothing to clear");
    }

    #[test]
    fn identically_labelled_rows_decline_and_their_session_is_withdrawn() {
        let rows = [dash(), DesiredLabel { row: "other/dash".into(), ..row("dash", Some("Ship it")) }];
        let p = fresh(&rows, &[target("k", Some("🔵 dash"), Some("Old task"))]);
        assert_eq!(p.declined, vec![Declined { row: "dash".into(), reason: "same_label", n: 2 }, Declined { row: "other/dash".into(), reason: "same_label", n: 2 }]);
        assert_eq!(p.writes, vec![withdraw("k", "Old task")]);
    }

    #[test]
    fn two_sessions_titled_for_one_row_decline_and_are_withdrawn() {
        // A shell left in a pane after its agent exited keeps the agent's title.
        let p = fresh(&[dash()], &[target("a", Some("🔵 dash"), Some("Fix the build")), target("b", Some("🟢 dash"), None)]);
        assert_eq!(p.declined, vec![Declined { row: "dash".into(), reason: "shared_title", n: 2 }]);
        assert_eq!(p.writes, vec![withdraw("a", "Fix the build")]);
    }

    #[test]
    fn a_title_of_ours_naming_no_live_row_has_its_context_withdrawn() {
        let p = fresh(&[], &[target("k", Some("⚪ gone [61%]"), Some("Fix the build"))]);
        assert_eq!(p.writes, vec![withdraw("k", "Fix the build")]);
        assert_eq!(fresh(&[], &[target("k", Some("⚪ gone"), None)]), Plan::default(), "nothing to clear");
    }

    #[test]
    fn a_context_on_a_session_whose_title_is_not_ours_is_left_alone() {
        let targets = [target("a", Some("~/p/dash — zsh"), Some("refactor sync")), target("b", None, Some("Fix the build"))];
        assert_eq!(fresh(&[], &targets), Plan::default());
        assert_eq!(fresh(&[dash()], &targets), Plan::default(), "even text equal to a row's task, which this process did not write there");
    }

    #[test]
    fn text_this_process_wrote_for_a_row_that_left_is_withdrawn_whatever_the_title() {
        // The row's title was blanked when it left, or titles were turned off.
        let written = HashMap::from([("k".to_string(), placed("dash", "Fix the build"))]);
        let t = target("k", None, Some("Fix the build"));
        assert_eq!(plan(&[], std::slice::from_ref(&t), &written).writes, vec![withdraw("k", "Fix the build")]);
        assert_eq!(plan(&[dash()], std::slice::from_ref(&t), &written), Plan::default(), "the row is still here, its title only elsewhere for now, as in a split whose shell pane has focus");
        assert_eq!(plan(&[], &[target("k", None, Some("Reviewing PR 12"))], &written), Plan::default(), "a context changed since it was written is not ours");
    }

    #[test]
    fn a_context_is_fitted_to_the_budget_its_terminal_reports() {
        // Its read-back of the fitted text compares equal, so it is not rewritten
        // every pass.
        let rows = [row("dash", Some("Fix the build and run the tests"))];
        let short = |shown: Option<&str>| LabelTarget { budget: LabelBudget::Utf16(10), ..target("k", Some("🔵 dash"), shown) };
        assert_eq!(fresh(&rows, &[short(None)]).writes, vec![write("dash", "k", "Fix the b…", None)]);
        assert_eq!(fresh(&rows, &[short(Some("Fix the b…"))]), Plan { in_place: vec![("k".into(), placed("dash", "Fix the b…"))], ..Plan::default() });
    }

    #[test]
    fn one_line_collapses_to_one_line() {
        assert_eq!(one_line("  Fix the\n\tbuild\r\n  now  ").as_deref(), Some("Fix the build now"));
        assert_eq!(one_line("a\u{7}b\u{1b}[0mc\u{85}d").as_deref(), Some("a b [0mc d"), "control characters, C1 included, become spaces");
        assert_eq!(one_line("a \u{a0}\u{2003} b").as_deref(), Some("a b"), "every whitespace run, not only ASCII");
        let long = "x".repeat(BUDGET + 50);
        assert_eq!(one_line(&long), Some(long.clone()), "no budget is applied here");
    }

    #[test]
    fn one_line_of_nothing_is_none() {
        assert_eq!(one_line(""), None);
        assert_eq!(one_line(" \n\t\r "), None);
        assert_eq!(one_line("\u{7}\u{0}"), None, "only control characters");
    }

    #[test]
    fn fit_utf16_keeps_a_line_at_the_ceiling_whole() {
        let at = "x".repeat(BUDGET);
        assert_eq!(fit_utf16(&at, BUDGET), at);
        let pairs = "😀".repeat(BUDGET / 2);
        assert_eq!(fit_utf16(&pairs, BUDGET), pairs, "100 surrogate pairs are exactly 200 units");
        let accented = "é".repeat(BUDGET);
        assert_eq!(fit_utf16(&accented, BUDGET), accented, "counted in UTF-16 units, not UTF-8 bytes");
    }

    #[test]
    fn fit_utf16_cuts_past_the_ceiling_and_ends_with_an_ellipsis() {
        let cut = fit_utf16(&"x".repeat(BUDGET + 1), BUDGET);
        assert_eq!(cut, format!("{}…", "x".repeat(BUDGET - 1)));
        assert_eq!(cut.encode_utf16().count(), BUDGET);
        assert_eq!(fit_utf16(&"é".repeat(300), BUDGET), format!("{}…", "é".repeat(BUDGET - 1)));
        assert_eq!(fit_utf16("abcdef", 4), "abc…", "whatever budget the terminal reports");
    }

    #[test]
    fn fit_utf16_never_splits_a_surrogate_pair() {
        // 198 units, then a pair that would end at 200: with a unit kept for the
        // ellipsis it does not fit, so it goes whole rather than leaving half.
        let cut = fit_utf16(&format!("{}😀tail", "x".repeat(198)), BUDGET);
        assert_eq!(cut, format!("{}…", "x".repeat(198)));
        assert!(cut.encode_utf16().count() <= BUDGET);
        assert_eq!(fit_utf16(&"😀".repeat(150), BUDGET), format!("{}…", "😀".repeat(99)), "99 pairs and the ellipsis are 199 units; a 100th would be 201");
    }

    #[test]
    fn fit_utf16_does_not_leave_a_space_before_the_ellipsis() {
        assert_eq!(fit_utf16(&format!("{} {}", "x".repeat(198), "y".repeat(10)), BUDGET), format!("{}…", "x".repeat(198)));
    }

    /// agterm's budget is bytes, so the unit is what these pin: the same line is
    /// inside a 256-unit budget and outside a 256-byte one once it stops being
    /// ASCII, which is the mistake that would have every Cyrillic task refused.
    #[test]
    fn fit_utf8_counts_bytes_and_not_characters() {
        const MAX: usize = 256;
        let cyrillic = "ф".repeat(200); // 400 bytes, 200 UTF-16 units
        assert_eq!(fit_utf16(&cyrillic, MAX), cyrillic, "inside a 256-unit budget");
        let fitted = fit_utf8(&cyrillic, MAX);
        assert!(fitted.len() <= MAX, "{} bytes is over the byte budget", fitted.len());
        assert!(fitted.ends_with('…'));
    }

    #[test]
    fn fit_utf8_keeps_a_line_at_the_ceiling_whole() {
        let line = "x".repeat(256);
        assert_eq!(fit_utf8(&line, 256), line);
    }

    #[test]
    fn fit_utf8_never_splits_a_character() {
        // 4-byte characters against a budget that is not a multiple of 4, so a
        // naive byte slice would land mid-character and panic.
        let line = "🔵".repeat(20);
        let fitted = fit_utf8(&line, 30);
        assert!(fitted.len() <= 30);
        assert!(fitted.chars().all(|c| c == '🔵' || c == '…'));
    }

    /// The one that read-back equality rests on: agterm stores the TRIMMED value,
    /// so a fitted line ending in a space would never equal what it reads back
    /// and `plan` would rewrite it on every pass for the life of the session.
    #[test]
    fn fit_utf8_leaves_no_trailing_space_either_way() {
        let cut = fit_utf8(&format!("{} {}", "x".repeat(250), "y".repeat(40)), 256);
        assert!(!cut.trim_end_matches('…').ends_with(' '), "{cut:?} would never match its own read-back");
        assert_eq!(fit_utf8("short task   ", 256), "short task", "a line inside the budget is trimmed too");
    }

    /// A terminal that records each write asked of it, and fails the ones whose
    /// key it is told to.
    struct Recording {
        targets: Vec<LabelTarget>,
        failing: Option<&'static str>,
        writes: Mutex<Vec<(String, LabelWrite)>>,
    }

    impl Recording {
        fn new(targets: Vec<LabelTarget>) -> Self {
            Recording { targets, failing: None, writes: Mutex::default() }
        }
    }

    impl TerminalAdapter for Recording {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn sessions(&self) -> Option<Vec<crate::terminals::TerminalSession>> {
            None
        }
        fn poll(&mut self, _now_ms: i64) -> Vec<crate::terminals::Observation> {
            Vec::new()
        }
        fn label_targets(&self) -> Option<Vec<LabelTarget>> {
            Some(self.targets.clone())
        }
        fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
            self.writes.lock().unwrap().push((key.to_string(), write.clone()));
            if self.failing == Some(key) {
                Err("refused".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_context_written_for_a_row_is_withdrawn_once_the_row_leaves_and_its_title_is_blanked() {
        let mut written = HashMap::new();
        let shown = Recording::new(vec![target("k", Some("🔵 dash"), None)]);
        assert_eq!(pass(&shown, &[dash()], &mut Seen::default(), &mut written), Outcome::Applied);
        assert_eq!(written.get("k"), Some(&placed("dash", "Fix the build")));
        let left = Recording::new(vec![target("k", None, Some("Fix the build"))]);
        assert_eq!(pass(&left, &[], &mut Seen::default(), &mut written), Outcome::Applied);
        assert_eq!(*left.writes.lock().unwrap(), vec![("k".to_string(), LabelWrite::ClearContext)]);
        assert!(written.is_empty(), "a cleared context is no longer recorded");
    }

    #[test]
    fn a_context_found_in_place_is_recorded_as_written() {
        // After a restart the row's context is already there, so nothing is
        // written for it, and its withdrawal still has the record to go by.
        let mut written = HashMap::new();
        let found = Recording::new(vec![target("k", Some("🟢 dash"), Some("Fix the build"))]);
        assert_eq!(pass(&found, &[dash()], &mut Seen::default(), &mut written), Outcome::Unchanged);
        assert!(found.writes.lock().unwrap().is_empty());
        assert_eq!(written.get("k"), Some(&placed("dash", "Fix the build")));
    }

    #[test]
    fn a_failed_write_ends_the_pass() {
        let mut terminal = Recording::new(vec![target("k1", Some("🔵 dash"), None), target("k2", Some("🔵 web"), None)]);
        terminal.failing = Some("k1");
        let mut written = HashMap::new();
        assert_eq!(pass(&terminal, &[dash(), row("web", Some("Ship it"))], &mut Seen::default(), &mut written), Outcome::WriteFailed);
        assert_eq!(terminal.writes.lock().unwrap().len(), 1, "the second write is left to the retry");
        assert!(written.is_empty());
    }
}
