//! Which live Claude Code sessions belong to a row, and which of them drives it.
//!
//! A row id is derived from the working directory, so every Claude Code process
//! in one folder addresses the same row. A row that followed whichever process
//! wrote last, judged by one recorded pid, would be taken by any short-lived
//! second process in the folder (a probe, a headless check), and when that
//! process exited the liveness reaper would judge the row by the dead pid and
//! remove a row whose real session was still working.
//!
//! So a row keeps the set of processes in it, its *members*, and one *main*.
//! Only the main's events change what the row shows; anyone else's are recorded
//! and logged as `member_event`. The election rules follow from how the folder
//! is used: one session per folder is the norm and two lasting ones are a fault.
//!
//! - The first member of an empty set is main (`founding`).
//! - Whenever exactly one member remains it is main, prompted or not
//!   (`succession` when the main left, `sole_member` when there was no main).
//! - The main leaving with two or more members still present leaves no main
//!   (`departed_no_successor`). With no main every member's events drive the row,
//!   last writer wins, until the set drops to one.
//!
//! Nothing else moves the main, a prompt included. A headless `claude -p` run
//! from inside a session fires `SessionStart`, a prompt, `Stop` and
//! `SessionEnd` from a second process in the same folder; were a prompt to
//! elect its sender, that probe would take the row, and its exit would settle
//! the real, still-working session `Done`. A second session the user types into
//! leaves the row following the first, and the shared-row alert reports the
//! overlap.
//!
//! A member is keyed by its owning pid where the hook resolved one, else by its
//! session id. The pid is what the reaper can judge and what survives a `/clear`
//! (which keeps the process and mints a new session id); the session id is what
//! an end signal carries, since the hook cannot resolve the pid of a process
//! that is shutting down. That is why [`RowMembers::depart`] matches on the
//! session id, the pid only choosing between two members that share one.
//!
//! Only pid-keyed members count toward sharing and succession on a row that has
//! ever had one. A session-keyed member names no process, so nothing can see it
//! leave without a `SessionEnd`, which Claude Code often does not send; counted,
//! one such member would hold the shared-row alert up for good and could be
//! handed the row when the real main exits. On an install whose hook resolves no
//! pid at all (node-based), every member is session-keyed and the sole-member
//! rule applies to them as it would to pids. What remains there is the reaper's
//! existing blind spot: a session-keyed member that exits silently stays, so a
//! row it shares is settled rather than removed when its other member leaves.
//!
//! The store is separate from `AgentSession` because membership has to outlive
//! the row through the gap between a `/clear`'s `SessionEnd` and its
//! `SessionStart`, and because none of it belongs on the sync wire, in
//! `prompt_history.json` or in `/api/agents`. In memory only: after a restart
//! `session_restore` seeds it from Claude Code's session registry, and anything
//! it misses is founded again by the session's next event.
//!
//! Locking: every write happens under the row's `commands::RowLocks` entry, which
//! is what orders a reap against a join. The inner mutex is a leaf, never held
//! across a call out of this module.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::session_registry::RecordKey;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MemberKey {
    Pid(u32),
    Session(String),
}

impl fmt::Display for MemberKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberKey::Pid(pid) => write!(f, "pid:{pid}"),
            MemberKey::Session(sid) => write!(f, "session:{sid}"),
        }
    }
}

#[derive(Clone, Debug)]
struct Member {
    key: MemberKey,
    /// `None` only for a record `session_restore` seeded without one.
    session_id: Option<String>,
    /// The id this member carried before its last rotation, which is how the
    /// late `SessionEnd` of a `/clear` whose `SessionStart` got here first is
    /// recognised.
    previous_session_id: Option<String>,
    /// A `SessionEnd` with `reason == "clear"` has arrived and its
    /// `SessionStart` has not. While set, that start does no teardown, because
    /// the end already did it.
    clearing: bool,
    /// The newest transcript this member reported, where the watcher goes when
    /// the member becomes main without an event of its own to start it.
    transcript_path: Option<PathBuf>,
}

/// One row's members. Pure; [`Members`] wraps a map of these in a lock.
#[derive(Clone, Debug, Default)]
pub struct RowMembers {
    members: Vec<Member>,
    main: Option<MemberKey>,
    /// The member whose event last changed what the row shows. With a main it
    /// is the main; with none it is whichever member wrote last, and its leaving
    /// takes the row's state with it as the main's would.
    last_driver: Option<MemberKey>,
    /// The row was restored from several registry records, so what it shows was
    /// written by one of them and nothing says which: the terminal reports no
    /// pid for the tab the status was read off. Cleared by the first event that
    /// drives the row, which makes its sender the known writer. While set, the
    /// first member to leave hands the row over, which settles a live question
    /// to an unread `Done` when the leaver was not its writer; that still asks
    /// to be looked at, where keeping a dead session's `Blocked` or `Working`
    /// would show a question nobody can answer or hold off sleep.
    restored_writer_unknown: bool,
    /// Whether the row has ever had a pid-keyed member, which is what makes a
    /// session-keyed one a member nothing can judge rather than the norm.
    pid_seen: bool,
    /// When the set last grew to two or more pid-keyed members, `None` below
    /// that. Kept here, beside the count it describes, so the shared-row alert
    /// reads the one record of it; a drop from three to two keeps the original
    /// instant.
    shared_since: Option<i64>,
}

/// The facts about one hook event that membership reads.
pub struct EventFacts<'a> {
    pub event: &'a str,
    /// `SessionStart`'s `source`.
    pub source: Option<&'a str>,
    pub pid: Option<u32>,
    /// Empty where the payload carried none.
    pub session_id: &'a str,
    pub transcript_path: Option<&'a Path>,
    /// Whether a pid is still a live Claude Code process, asked only when a
    /// start would move a member off that pid. `true` where it cannot be told:
    /// a wrong `true` costs an extra member and the shared-row alert, a wrong
    /// `false` hands the main's office to another process.
    pub still_running: &'a dyn Fn(u32) -> bool,
}

impl EventFacts<'_> {
    fn sid(&self) -> Option<&str> {
        (!self.session_id.is_empty()).then_some(self.session_id)
    }

    fn is_start(&self) -> bool {
        self.event == "SessionStart"
    }

    fn is_clear_start(&self) -> bool {
        self.is_start() && self.source == Some("clear")
    }
}

/// Why the main changed, logged as `main_change`'s `via`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Founding,
    Succession,
    SoleMember,
    DepartedNoSuccessor,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Founding => "founding",
            Via::Succession => "succession",
            Via::SoleMember => "sole_member",
            Via::DepartedNoSuccessor => "departed_no_successor",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Via::Founding => "the first session seen on this row drives it",
            Via::Succession => "the main session left and one session remains, so it drives the row now",
            Via::SoleMember => "only one session remains in this folder, so it drives the row now",
            Via::DepartedNoSuccessor => "the main session left and several remain, so each drives the row until only one remains",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainChange {
    pub from: Option<MemberKey>,
    pub to: Option<MemberKey>,
    pub via: Via,
}

/// A known member arriving under a new session id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rotation {
    pub from: Option<String>,
    /// This is a `/clear` start that reached the server before its end, on a
    /// member that drives the row, so the start has to do the end's teardown.
    pub teardown: bool,
}

/// What admitting one event did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Admission {
    /// The sender, or `None` for an event carrying neither a pid nor a session id.
    pub member: Option<MemberKey>,
    pub joined: bool,
    /// The key the sender was known by before this event, when it changed.
    pub rekeyed: Option<MemberKey>,
    /// Whether this event may change what the row shows.
    pub drives_row: bool,
    pub rotation: Option<Rotation>,
    pub main_change: Option<MainChange>,
    /// The main once the event is admitted.
    pub main: Option<MemberKey>,
}

impl Admission {
    pub fn is_main(&self) -> bool {
        self.member.is_some() && self.member == self.main
    }
}

/// A member removed from the set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Left {
    pub was_main: bool,
    /// The departed member is the one whose state the row shows: the main, or
    /// the last writer while there was none.
    pub drove_last: bool,
    pub remaining: usize,
    pub main_change: Option<MainChange>,
}

impl Left {
    /// Whether the row has to be handed over (`AppState::hand_over`): what it
    /// shows is the departed member's.
    pub fn hands_over(&self) -> bool {
        self.was_main || self.drove_last
    }
}

/// What a `SessionEnd` means for the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Departure {
    /// The end of a `/clear` whose start already did the teardown.
    Superseded,
    /// A `/clear` end: the member stays, since the same process starts again.
    /// `drives_row` is whether the ending session drives the row (it is main,
    /// or there is none), which is whether the end tears the row down.
    Clearing { drives_row: bool },
    Left(Left),
    /// No member carries that session id. `row_known` is whether the row has
    /// any members at all: with none there is nothing to be wrong about, and the
    /// authoritative end signal wins.
    Unmatched { row_known: bool },
}

/// Whether `commands::remove_session` keeps the row's members. A `/clear` keeps
/// them, because the process that ended is the one about to start again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    Keep,
    Forget,
}

impl RowMembers {
    fn position(&self, pred: impl Fn(&Member) -> bool) -> Option<usize> {
        self.members.iter().position(pred)
    }

    fn rekey(&mut self, i: usize, to: MemberKey, adm: &mut Admission) {
        let from = std::mem::replace(&mut self.members[i].key, to.clone());
        for held in [&mut self.main, &mut self.last_driver] {
            if held.as_ref() == Some(&from) {
                *held = Some(to.clone());
            }
        }
        adm.rekeyed = Some(from);
    }

    /// The member a `/clear` start belongs to when its new session id matches
    /// nobody: the one member awaiting its clear start, else the main when the
    /// start's key kind differs from the main's (a pid-less start on a pid-keyed
    /// main, or any start on a main that has never reported a pid). Without it a
    /// session whose hook resolved its pid on one event and not the other would
    /// see its own `/clear` join as a stranger, and the row would stop following
    /// it. A start carrying a pid other than a pid-keyed main's is a different
    /// process and is not matched.
    fn clear_start_target(&self, pid: Option<u32>) -> Option<usize> {
        let awaiting: Vec<usize> = (0..self.members.len()).filter(|&i| self.members[i].clearing && (pid.is_none() || matches!(self.members[i].key, MemberKey::Session(_)))).collect();
        if let [only] = awaiting.as_slice() {
            return Some(*only);
        }
        match (&self.main, pid) {
            (Some(main @ MemberKey::Session(_)), _) | (Some(main @ MemberKey::Pid(_)), None) => self.position(|m| &m.key == main),
            _ => None,
        }
    }

    /// Whether `m` may be made main by the set narrowing to it: a pid-keyed
    /// member always, a session-keyed one only on a row that has never had a pid
    /// (see the module docs).
    fn may_succeed(&self, m: &Member) -> bool {
        matches!(m.key, MemberKey::Pid(_)) || !self.pid_seen
    }

    /// Find the sender, upgrading or rekeying it where the event says how, and
    /// failing that add it.
    fn locate(&mut self, f: &EventFacts, adm: &mut Admission) -> Option<usize> {
        let sid = f.sid();
        if let Some(pid) = f.pid {
            if let Some(i) = self.position(|m| m.key == MemberKey::Pid(pid)) {
                return Some(i);
            }
            if let Some(s) = sid {
                // The same session reporting its pid for the first time.
                if let Some(i) = self.position(|m| matches!(m.key, MemberKey::Session(_)) && m.session_id.as_deref() == Some(s)) {
                    self.rekey(i, MemberKey::Pid(pid), adm);
                    return Some(i);
                }
                // A session resumed in a new process after the old one exited
                // and before it was reaped: `--continue` and `--resume` keep the
                // session id. Only a start may do this, since that is the only
                // event a new process opens with, and only off a process that is
                // gone: a `claude -p --continue` run from inside the session, or
                // a second terminal's `--continue`, shares the id while the
                // original still runs, and moving the key would hand it the main.
                if f.is_start() {
                    if let Some(i) = self.position(|m| matches!(m.key, MemberKey::Pid(old) if !(f.still_running)(old)) && m.session_id.as_deref() == Some(s)) {
                        self.rekey(i, MemberKey::Pid(pid), adm);
                        return Some(i);
                    }
                }
            }
        } else if let Some(s) = sid {
            if let Some(i) = self.position(|m| m.session_id.as_deref() == Some(s) || m.previous_session_id.as_deref() == Some(s)) {
                return Some(i);
            }
        }
        if f.is_clear_start() {
            if let Some(i) = self.clear_start_target(f.pid) {
                if let Some(pid) = f.pid {
                    self.rekey(i, MemberKey::Pid(pid), adm);
                }
                return Some(i);
            }
        }
        let key = match (f.pid, sid) {
            (Some(pid), _) => MemberKey::Pid(pid),
            (None, Some(s)) => MemberKey::Session(s.to_string()),
            (None, None) => return None,
        };
        self.members.push(Member { key, session_id: sid.map(str::to_string), previous_session_id: None, clearing: false, transcript_path: None });
        adm.joined = true;
        Some(self.members.len() - 1)
    }

    pub fn admit(&mut self, f: &EventFacts) -> Admission {
        let mut adm = Admission::default();
        let was_empty = self.members.is_empty();
        let Some(i) = self.locate(f, &mut adm) else {
            adm.drives_row = self.main.is_none();
            adm.main = self.main.clone();
            return adm;
        };
        let m = &mut self.members[i];
        if let Some(path) = f.transcript_path {
            m.transcript_path = Some(path.to_path_buf());
        }
        // Never back to the id this member just left: a late event from before
        // the rotation must not undo it.
        let rotating = !adm.joined && f.sid().is_some_and(|s| m.session_id.as_deref() != Some(s) && m.previous_session_id.as_deref() != Some(s));
        let clear_start_pending = f.is_clear_start() && !m.clearing;
        let rotated_from = rotating.then(|| std::mem::replace(&mut m.session_id, f.sid().map(str::to_string)));
        if let Some(from) = &rotated_from {
            m.previous_session_id = from.clone();
        }
        if f.is_start() {
            m.clearing = false;
        }
        let key = m.key.clone();
        if matches!(key, MemberKey::Pid(_)) {
            self.pid_seen = true;
        }
        if was_empty {
            self.main = Some(key.clone());
            adm.main_change = Some(MainChange { from: None, to: Some(key.clone()), via: Via::Founding });
        } else if self.main.is_none() {
            // The first process to join a row held only by session-keyed strays
            // is its one judged member, which the sole-member rule makes main.
            adm.main_change = self.settle_main(false);
        }
        adm.drives_row = self.main.is_none() || self.main.as_ref() == Some(&key);
        if adm.drives_row {
            self.last_driver = Some(key.clone());
            self.restored_writer_unknown = false;
        }
        adm.rotation = rotated_from.map(|from| Rotation { from, teardown: clear_start_pending && adm.drives_row });
        adm.member = Some(key);
        adm.main = self.main.clone();
        adm
    }

    /// Re-establish the election after members left: a sole member that may
    /// succeed is main, and a main that left takes the office with it.
    fn settle_main(&mut self, main_left: bool) -> Option<MainChange> {
        let from = if main_left {
            // The row's state was the departed main's, and `apply_departure`
            // settles it, so no remaining member is its writer.
            self.last_driver = None;
            self.main.take()
        } else {
            None
        };
        if self.main.is_none() {
            let mut eligible = self.members.iter().filter(|m| self.may_succeed(m));
            if let (Some(only), None) = (eligible.next(), eligible.next()) {
                let to = only.key.clone();
                let via = if main_left { Via::Succession } else { Via::SoleMember };
                self.main = Some(to.clone());
                return Some(MainChange { from, to: Some(to), via });
            }
        }
        (main_left && !self.members.is_empty()).then_some(MainChange { from, to: None, via: Via::DepartedNoSuccessor })
    }

    /// Whether the member keyed `k` is the row's last writer, or may be because
    /// the restored writer is unknown, clearing both records, since the member
    /// is about to leave.
    fn take_last_driver(&mut self, k: impl Fn(&MemberKey) -> bool) -> bool {
        let drove = self.last_driver.as_ref().is_some_and(k);
        if drove {
            self.last_driver = None;
        }
        std::mem::take(&mut self.restored_writer_unknown) || drove
    }

    /// The member a `SessionEnd` ends. Matched by session id, since the hook
    /// resolves no pid for a process that is shutting down; the pid, where the
    /// end carries one, only chooses between members sharing that id. With two
    /// sharing it and no pid the one that is not main goes: removing the main
    /// wrongly settles a working row, while removing the other wrongly leaves
    /// the real leaver for the reaper to drop once its process is seen dead.
    fn ending_member(&self, session_id: &str, pid: Option<u32>) -> Option<usize> {
        let carries = |m: &Member| m.session_id.as_deref() == Some(session_id);
        if let Some(pid) = pid {
            if let Some(i) = self.position(|m| carries(m) && m.key == MemberKey::Pid(pid)) {
                return Some(i);
            }
        }
        self.members.iter().rposition(|m| carries(m) && self.main.as_ref() != Some(&m.key)).or_else(|| self.position(carries))
    }

    pub fn depart(&mut self, session_id: &str, pid: Option<u32>, wiped: bool) -> Departure {
        let row_known = !self.members.is_empty();
        if session_id.is_empty() {
            return Departure::Unmatched { row_known };
        }
        if let Some(i) = self.ending_member(session_id, pid) {
            let was_main = self.main.as_ref() == Some(&self.members[i].key);
            if wiped {
                self.members[i].clearing = true;
                return Departure::Clearing { drives_row: was_main || self.main.is_none() };
            }
            let gone = self.members.remove(i).key;
            let drove_last = self.take_last_driver(|k| k == &gone);
            let main_change = self.settle_main(was_main);
            return Departure::Left(Left { was_main, drove_last, remaining: self.members.len(), main_change });
        }
        if self.members.iter().any(|m| m.previous_session_id.as_deref() == Some(session_id)) {
            return Departure::Superseded;
        }
        Departure::Unmatched { row_known }
    }

    /// Remove exactly the pids the reaper judged dead, so a member that joined
    /// since its last read keeps the row. `None` when none of them is a member.
    pub fn drop_dead(&mut self, pids: &[u32]) -> Option<Left> {
        let dead = |k: &MemberKey| matches!(k, MemberKey::Pid(p) if pids.contains(p));
        let was_main = self.main.as_ref().is_some_and(dead);
        let before = self.members.len();
        self.members.retain(|m| !dead(&m.key));
        if self.members.len() == before {
            return None;
        }
        let drove_last = self.take_last_driver(dead);
        let main_change = self.settle_main(was_main);
        Some(Left { was_main, drove_last, remaining: self.members.len(), main_change })
    }

    /// Seed a restored row's members from the session registry. Only into an
    /// empty set: a hook event that got here first knows more than the registry.
    pub fn seed(&mut self, records: &[RecordKey]) -> bool {
        if !self.members.is_empty() || records.is_empty() {
            return false;
        }
        self.members = records.iter().map(|r| Member { key: MemberKey::Pid(r.pid), session_id: r.session_id.clone(), previous_session_id: None, clearing: false, transcript_path: None }).collect();
        self.pid_seen = true;
        if let [only] = self.members.as_slice() {
            self.main = Some(only.key.clone());
        } else {
            self.restored_writer_unknown = true;
        }
        true
    }

    /// Bring `shared_since` in line with the pid-keyed member count. Every
    /// [`Members`] wrapper that can change the count calls it, which is what
    /// keeps the instant and the count from disagreeing. Session-keyed members
    /// are not counted: nothing can ever prove one gone (see the module docs),
    /// so counting it would hold the row shared after its session had exited.
    fn note_sharing(&mut self, now: i64) {
        if self.members.iter().filter(|m| matches!(m.key, MemberKey::Pid(_))).count() >= 2 {
            self.shared_since.get_or_insert(now);
        } else {
            self.shared_since = None;
        }
    }

    fn main_member(&self) -> Option<&Member> {
        let main = self.main.as_ref()?;
        self.members.iter().find(|m| &m.key == main)
    }
}

pub(crate) fn key_text(k: Option<&MemberKey>) -> String {
    k.map_or_else(|| "none".to_string(), ToString::to_string)
}

/// The `main_change` line.
pub fn log_main_change(row: &str, c: &MainChange) {
    tracing::debug!(chat_id = %row, decision = "main_change", from = %key_text(c.from.as_ref()), to = %key_text(c.to.as_ref()), via = c.via.as_str(), reason = c.via.reason(), "row main changed");
}

/// The membership lines one admitted event produces: `member_join`,
/// `member_rekey` and `main_change`, each only where it happened.
pub fn log_admission(row: &str, event: &str, session_id: &str, adm: &Admission) {
    let member = key_text(adm.member.as_ref());
    if adm.joined {
        tracing::debug!(chat_id = %row, decision = "member_join", event, member = %member, session_id, main = adm.is_main(), "a session joined the row");
    }
    if let Some(from) = &adm.rekeyed {
        tracing::debug!(chat_id = %row, decision = "member_rekey", event, from = %from, to = %member, session_id, "a member is now known by another key");
    }
    if let Some(c) = &adm.main_change {
        log_main_change(row, c);
    }
}

/// Every row's members, managed as Tauri state.
#[derive(Default)]
pub struct Members {
    rows: Mutex<HashMap<String, RowMembers>>,
}

impl Members {
    /// Admit one event into `row`. `Err` names the row the event's pid is
    /// already a member of, and changes nothing: the caller chose `row` before
    /// taking its lock, and two events from one process anchored to different
    /// rows can both find the pid in no row at that moment. Checked here, under
    /// the lock that covers every row, because this is where a pid key is
    /// written; the caller retries under the named row's own lock.
    pub fn admit(&self, row: &str, facts: &EventFacts, now: i64) -> Result<Admission, String> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(pid) = facts.pid {
            if let Some(other) = rows.iter().find(|(id, r)| id.as_str() != row && r.members.iter().any(|m| m.key == MemberKey::Pid(pid))).map(|(id, _)| id.clone()) {
                return Err(other);
            }
        }
        let r = rows.entry(row.to_string()).or_default();
        let adm = r.admit(facts);
        r.note_sharing(now);
        Ok(adm)
    }

    pub fn depart(&self, row: &str, session_id: &str, pid: Option<u32>, wiped: bool, now: i64) -> Departure {
        match self.rows.lock().unwrap().get_mut(row) {
            Some(r) => {
                let departure = r.depart(session_id, pid, wiped);
                r.note_sharing(now);
                departure
            }
            None => Departure::Unmatched { row_known: false },
        }
    }

    pub fn drop_dead(&self, row: &str, pids: &[u32], now: i64) -> Option<Left> {
        let mut rows = self.rows.lock().unwrap();
        let r = rows.get_mut(row)?;
        let left = r.drop_dead(pids);
        r.note_sharing(now);
        left
    }

    pub fn seed(&self, row: &str, records: &[RecordKey], now: i64) {
        let mut rows = self.rows.lock().unwrap();
        let r = rows.entry(row.to_string()).or_default();
        r.seed(records);
        r.note_sharing(now);
    }

    /// Every row with two or more members, and since when, for the shared-row
    /// alert in `notifications`.
    pub fn shared_since(&self) -> HashMap<String, i64> {
        self.rows.lock().unwrap().iter().filter_map(|(id, r)| Some((id.clone(), r.shared_since?))).collect()
    }

    /// The row `pid` is a member of, read before the row lock is chosen. A hint
    /// only: a pid can join another row between this read and the lock, and
    /// [`Members::admit`] is what refuses that.
    pub fn row_of_pid(&self, pid: u32) -> Option<String> {
        let rows = self.rows.lock().unwrap();
        rows.iter().find(|(_, r)| r.members.iter().any(|m| m.key == MemberKey::Pid(pid))).map(|(id, _)| id.clone())
    }

    /// The main's current session id, where there is a main and it has one.
    pub fn main_session(&self, row: &str) -> Option<String> {
        self.rows.lock().unwrap().get(row)?.main_member()?.session_id.clone()
    }

    pub fn main_transcript(&self, row: &str) -> Option<PathBuf> {
        self.rows.lock().unwrap().get(row)?.main_member()?.transcript_path.clone()
    }

    /// Every pid-keyed member with its row, for the reaper. A session-keyed
    /// member names no process, so nothing can judge it.
    pub fn pid_members(&self) -> Vec<(String, u32)> {
        let rows = self.rows.lock().unwrap();
        rows.iter().flat_map(|(id, r)| r.members.iter().filter_map(move |m| match m.key {
            MemberKey::Pid(pid) => Some((id.clone(), pid)),
            MemberKey::Session(_) => None,
        })).collect()
    }

    pub fn pids(&self, row: &str) -> Vec<u32> {
        self.pid_members().into_iter().filter(|(id, _)| id == row).map(|(_, pid)| pid).collect()
    }

    pub fn forget_row(&self, row: &str) {
        self.rows.lock().unwrap().remove(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every process the tests name has exited, unless a test says otherwise.
    fn gone(_: u32) -> bool {
        false
    }

    fn ev<'a>(event: &'a str, pid: Option<u32>, sid: &'a str) -> EventFacts<'a> {
        EventFacts { event, source: None, pid, session_id: sid, transcript_path: None, still_running: &gone }
    }

    fn start<'a>(source: &'a str, pid: Option<u32>, sid: &'a str) -> EventFacts<'a> {
        EventFacts { event: "SessionStart", source: Some(source), pid, session_id: sid, transcript_path: None, still_running: &gone }
    }

    const A: u32 = 34_390;
    const PROBE: u32 = 51_016;

    /// A row whose main is pid `A` under `s1`, mid-turn.
    fn with_main() -> RowMembers {
        let mut r = RowMembers::default();
        let adm = r.admit(&ev("UserPromptSubmit", Some(A), "s1"));
        assert_eq!(adm.main_change.map(|c| c.via), Some(Via::Founding));
        r
    }

    #[test]
    fn a_probe_started_in_the_folder_joins_without_driving_the_row() {
        // The incident: a second `claude` started in a working session's folder
        // for 13s. Its SessionStart overwrote the row and its death got the row
        // reaped while the real session was mid-turn.
        let mut r = with_main();
        let adm = r.admit(&start("startup", Some(PROBE), "probe"));
        assert!(adm.joined);
        assert!(!adm.drives_row, "a non-main member's event must not touch the row");
        assert!(!adm.is_main());
        assert_eq!(adm.main, Some(MemberKey::Pid(A)));
        assert!(!r.admit(&ev("Stop", Some(PROBE), "probe")).drives_row);
        let left = r.drop_dead(&[PROBE]).expect("the probe was a member");
        assert_eq!(left, Left { was_main: false, drove_last: false, remaining: 1, main_change: None }, "main is untouched, so nothing is removed");
        assert!(r.admit(&ev("Stop", Some(A), "s1")).drives_row);
    }

    #[test]
    fn a_probe_cannot_take_the_canary_nonce() {
        // `session_start_nonce` is handed `drives_row` (through
        // `http_server::set_effects`), so a probe's startup is given the row's
        // nonce and never mints over it.
        let mut r = with_main();
        assert!(!r.admit(&start("startup", Some(PROBE), "probe")).drives_row);
    }

    #[test]
    fn a_prompt_in_a_second_session_neither_moves_the_main_nor_drives_the_row() {
        for prompt in ["UserPromptSubmit", "UserPromptExpansion"] {
            let mut r = with_main();
            assert!(!r.admit(&start("startup", Some(PROBE), "second")).drives_row);
            let adm = r.admit(&ev(prompt, Some(PROBE), "second"));
            assert_eq!(adm.main_change, None, "{prompt}");
            assert!(!adm.drives_row && !adm.is_main(), "{prompt}");
            assert_eq!(adm.main, Some(MemberKey::Pid(A)));
            assert!(r.admit(&ev("Stop", Some(A), "s1")).drives_row, "the main's own Stop still drives the row");
        }
    }

    #[test]
    fn a_clear_whose_end_arrives_first_keeps_the_member_and_does_no_second_teardown() {
        let mut r = with_main();
        assert_eq!(r.depart("s1", None, true), Departure::Clearing { drives_row: true });
        assert_eq!(r.members.len(), 1, "the process is about to start again");
        let adm = r.admit(&start("clear", Some(A), "s2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: false }));
        assert!(adm.drives_row && adm.is_main());
    }

    #[test]
    fn a_clear_whose_start_arrives_first_does_the_teardown_and_its_late_end_is_superseded() {
        let mut r = with_main();
        let adm = r.admit(&start("clear", Some(A), "s2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: true }));
        assert_eq!(r.depart("s1", None, true), Departure::Superseded);
        assert!(r.admit(&ev("Stop", Some(A), "s1")).rotation.is_none(), "a late event from the old id does not rotate back");
    }

    #[test]
    fn a_non_main_members_clear_never_tears_the_row_down_in_either_order() {
        let mut r = with_main();
        r.admit(&start("startup", Some(PROBE), "t1"));
        assert_eq!(r.depart("t1", None, true), Departure::Clearing { drives_row: false });
        let adm = r.admit(&start("clear", Some(PROBE), "t2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("t1".into()), teardown: false }));
        assert!(!adm.drives_row);

        let mut r = with_main();
        r.admit(&start("startup", Some(PROBE), "t1"));
        let adm = r.admit(&start("clear", Some(PROBE), "t2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("t1".into()), teardown: false }), "not main, so not its teardown to do");
        assert_eq!(r.depart("t1", None, true), Departure::Superseded);
    }

    #[test]
    fn another_members_event_inside_the_clear_gap_does_not_drive() {
        let mut r = with_main();
        r.admit(&start("startup", Some(PROBE), "t1"));
        r.depart("s1", None, true);
        assert!(!r.admit(&ev("Notification", Some(PROBE), "t1")).drives_row);
    }

    #[test]
    fn an_end_with_no_pid_is_matched_by_its_session() {
        // The hook resolves no pid for a process that is shutting down.
        let mut r = with_main();
        assert_eq!(r.depart("s1", None, false), Departure::Left(Left { was_main: true, drove_last: true, remaining: 0, main_change: None }));
    }

    #[test]
    fn an_end_nobody_carries_is_refused_only_where_the_row_has_members() {
        let mut r = with_main();
        assert_eq!(r.depart("stranger", None, false), Departure::Unmatched { row_known: true });
        assert_eq!(r.depart("", None, false), Departure::Unmatched { row_known: true });
        assert_eq!(RowMembers::default().depart("stranger", None, false), Departure::Unmatched { row_known: false });
    }

    #[test]
    fn a_sole_survivor_succeeds_and_two_survivors_leave_no_main() {
        let mut r = with_main();
        r.admit(&start("startup", Some(PROBE), "t1"));
        assert_eq!(r.depart("s1", None, false), Departure::Left(Left { was_main: true, drove_last: true, remaining: 1, main_change: Some(MainChange { from: Some(MemberKey::Pid(A)), to: Some(MemberKey::Pid(PROBE)), via: Via::Succession }) }), "never prompted, and main all the same");

        let mut r = with_main();
        r.admit(&start("startup", Some(7), "b"));
        r.admit(&start("startup", Some(8), "c"));
        let left = r.drop_dead(&[A]).unwrap();
        assert_eq!(left.main_change, Some(MainChange { from: Some(MemberKey::Pid(A)), to: None, via: Via::DepartedNoSuccessor }));
        assert!(r.admit(&ev("Stop", Some(7), "b")).drives_row, "with no main every member drives, last writer wins");
        assert!(r.admit(&ev("Stop", Some(8), "c")).drives_row);
    }

    #[test]
    fn with_two_members_and_no_main_a_prompt_drives_the_row_and_elects_nobody() {
        let mut r = RowMembers::default();
        r.seed(&[rec(7, "b"), rec(8, "c")]);
        let adm = r.admit(&ev("UserPromptSubmit", Some(7), "b"));
        assert!(adm.drives_row);
        assert_eq!((adm.main_change, adm.main), (None, None));
        assert!(r.admit(&ev("Stop", Some(8), "c")).drives_row, "the other member still drives");
        assert_eq!(r.main, None);
    }

    #[test]
    fn the_set_dropping_to_one_with_no_main_makes_that_one_main() {
        let mut r = with_main();
        r.admit(&start("startup", Some(7), "b"));
        r.admit(&start("startup", Some(8), "c"));
        r.drop_dead(&[A]);
        let left = r.drop_dead(&[8]).unwrap();
        assert_eq!(left.main_change, Some(MainChange { from: None, to: Some(MemberKey::Pid(7)), via: Via::SoleMember }));
    }

    #[test]
    fn a_pid_reported_late_upgrades_the_member_and_keeps_it_main() {
        let mut r = RowMembers::default();
        r.admit(&ev("UserPromptSubmit", None, "s1"));
        let adm = r.admit(&ev("Stop", Some(A), "s1"));
        assert_eq!(adm.rekeyed, Some(MemberKey::Session("s1".into())));
        assert!(adm.is_main() && !adm.joined);
        assert_eq!(r.main, Some(MemberKey::Pid(A)));
    }

    #[test]
    fn only_a_start_carries_a_session_into_a_new_process() {
        // `claude --continue` keeps the session id and starts a new process.
        let mut r = with_main();
        let adm = r.admit(&start("resume", Some(PROBE), "s1"));
        assert_eq!(adm.rekeyed, Some(MemberKey::Pid(A)));
        assert!(adm.is_main() && !adm.joined);

        let mut r = with_main();
        let adm = r.admit(&ev("Stop", Some(PROBE), "s1"));
        assert!(adm.joined && adm.rekeyed.is_none(), "any other event is a separate process sharing the id");
        assert!(!adm.drives_row);
    }

    #[test]
    fn a_continue_beside_a_running_session_joins_and_its_end_leaves_the_main() {
        // `claude -p --continue` run from inside session A keeps A's id while A
        // is still mid-turn.
        let a_runs = |pid: u32| pid == A;
        let mut r = with_main();
        let adm = r.admit(&EventFacts { still_running: &a_runs, ..start("resume", Some(PROBE), "s1") });
        assert!(adm.joined && adm.rekeyed.is_none() && !adm.drives_row, "A still runs, so the main stays on it");
        assert_eq!(r.main, Some(MemberKey::Pid(A)));
        assert!(!r.admit(&ev("Stop", Some(PROBE), "s1")).drives_row);
        assert!(r.admit(&ev("Stop", Some(A), "s1")).drives_row);
        let leave = Departure::Left(Left { was_main: false, drove_last: false, remaining: 1, main_change: None });
        assert_eq!(r.clone().depart("s1", Some(PROBE), false), leave, "the end's pid names the probe");
        assert_eq!(r.depart("s1", None, false), leave, "with no pid the member that is not main goes");
        assert_eq!(r.main, Some(MemberKey::Pid(A)));
    }

    #[test]
    fn a_pid_less_sessions_own_clear_is_followed_in_either_order() {
        let mut r = RowMembers::default();
        r.admit(&ev("UserPromptSubmit", None, "s1"));
        let adm = r.admit(&start("clear", None, "s2"));
        assert!(!adm.joined && adm.is_main());
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: true }));
        assert_eq!(r.depart("s1", None, true), Departure::Superseded);

        let mut r = RowMembers::default();
        r.admit(&ev("UserPromptSubmit", None, "s1"));
        r.depart("s1", None, true);
        let adm = r.admit(&start("clear", None, "s2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: false }));
    }

    #[test]
    fn an_event_with_no_identity_drives_only_a_row_with_no_main() {
        let mut r = RowMembers::default();
        assert!(r.admit(&ev("Stop", None, "")).drives_row);
        assert!(r.members.is_empty(), "nothing to make a member of");
        let mut r = with_main();
        assert!(!r.admit(&ev("Stop", None, "")).drives_row);
    }

    #[test]
    fn a_seed_lands_only_in_an_empty_set_and_names_a_main_only_when_alone() {
        let rec = |pid, sid: &str| RecordKey { pid, session_id: Some(sid.into()) };
        let mut r = RowMembers::default();
        assert!(r.seed(&[rec(1, "a"), rec(2, "b")]));
        assert_eq!(r.main, None, "two collapsed records, and the speaker is not the owner");
        assert!(!r.seed(&[rec(3, "c")]));
        let mut r = RowMembers::default();
        assert!(r.seed(&[rec(1, "a")]));
        assert_eq!(r.main, Some(MemberKey::Pid(1)));
        let mut r = with_main();
        assert!(!r.seed(&[rec(9, "z")]), "a hook event got here first");
    }

    #[test]
    fn the_first_to_leave_a_row_restored_from_several_records_hands_it_over() {
        // Nothing says whose tab the restored status was read off.
        for gone in [1, 2] {
            let mut r = RowMembers::default();
            r.seed(&[rec(1, "a"), rec(2, "b")]);
            assert!(r.drop_dead(&[gone]).unwrap().hands_over(), "pid {gone}");
        }
        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b"), rec(3, "c")]);
        let Departure::Left(left) = r.depart("b", None, false) else { panic!("b was a member") };
        assert!(left.hands_over());
        assert!(!r.drop_dead(&[3]).unwrap().hands_over(), "the row was already handed over and nobody has written since");

        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b")]);
        r.admit(&ev("Stop", Some(1), "a"));
        assert!(!r.drop_dead(&[2]).unwrap().hands_over(), "a's own event made it the known writer");
    }

    #[test]
    fn drop_dead_removes_only_the_judged_pids() {
        // A member that joined between the reaper's reads and its removal keeps
        // the row: only the pids that read dead three times go.
        let mut r = with_main();
        r.admit(&start("startup", Some(PROBE), "probe"));
        r.admit(&start("startup", Some(9), "late"));
        let left = r.drop_dead(&[PROBE]).unwrap();
        assert_eq!(left.remaining, 2);
        assert_eq!(r.main, Some(MemberKey::Pid(A)));
        assert!(r.drop_dead(&[PROBE]).is_none(), "already gone");
    }

    #[test]
    fn the_store_finds_a_row_by_pid_and_lists_pid_members() {
        let m = Members::default();
        m.admit("dash", &ev("UserPromptSubmit", Some(A), "s1"), 0).unwrap();
        m.admit("dash", &ev("Stop", None, "s9"), 0).unwrap();
        m.admit("web", &ev("UserPromptSubmit", Some(5), "w"), 0).unwrap();
        assert_eq!(m.row_of_pid(A).as_deref(), Some("dash"));
        assert_eq!(m.row_of_pid(6), None);
        assert_eq!(m.pids("dash"), vec![A], "a session-keyed member names no process");
        assert_eq!(m.main_session("dash").as_deref(), Some("s1"));
        m.forget_row("dash");
        assert_eq!(m.row_of_pid(A), None);
        assert_eq!(m.depart("dash", "s1", None, false, 0), Departure::Unmatched { row_known: false });
    }

    #[test]
    fn a_row_is_shared_from_its_second_member_until_it_is_back_to_one() {
        let m = Members::default();
        m.admit("dash", &ev("UserPromptSubmit", Some(A), "s1"), 100).unwrap();
        assert!(m.shared_since().is_empty(), "one member is the norm");
        m.admit("dash", &start("startup", Some(PROBE), "probe"), 200).unwrap();
        assert_eq!(m.shared_since().get("dash"), Some(&200));
        m.admit("dash", &start("startup", Some(9), "third"), 300).unwrap();
        m.admit("dash", &ev("Stop", Some(PROBE), "probe"), 400).unwrap();
        assert_eq!(m.shared_since().get("dash"), Some(&200), "a third member and later events keep the first instant");
        m.drop_dead("dash", &[9], 500);
        assert_eq!(m.shared_since().get("dash"), Some(&200), "still two");
        m.depart("dash", "probe", None, false, 600);
        assert!(m.shared_since().is_empty(), "back to one");
        m.admit("dash", &start("startup", Some(PROBE), "again"), 700).unwrap();
        assert_eq!(m.shared_since().get("dash"), Some(&700), "a new overlap starts its own clock");
    }

    #[test]
    fn a_clear_in_progress_keeps_a_shared_row_shared() {
        // The clearing member stays a member, so the end of its `/clear` must
        // neither clear the instant nor restart it.
        let m = Members::default();
        m.admit("dash", &ev("UserPromptSubmit", Some(A), "s1"), 100).unwrap();
        m.admit("dash", &start("startup", Some(PROBE), "probe"), 200).unwrap();
        m.depart("dash", "s1", None, true, 300);
        assert_eq!(m.shared_since().get("dash"), Some(&200));
    }

    #[test]
    fn a_seed_of_several_records_is_shared_from_the_seed_and_a_forgotten_row_is_not() {
        let rec = |pid, sid: &str| RecordKey { pid, session_id: Some(sid.into()) };
        let m = Members::default();
        m.seed("dash", &[rec(1, "a"), rec(2, "b")], 50);
        assert_eq!(m.shared_since().get("dash"), Some(&50));
        m.forget_row("dash");
        assert!(m.shared_since().is_empty());
    }

    fn rec(pid: u32, sid: &str) -> RecordKey {
        RecordKey { pid, session_id: Some(sid.into()) }
    }

    #[test]
    fn probes_that_prompt_and_die_leave_the_main_alone() {
        // 2026-09-01, agterm: the main (pid 67866) was mid-turn when four probes
        // each started, sent one prompt ("config") within a second, and died
        // with no Stop and no SessionEnd.
        let mut r = with_main();
        for (probe, sid) in [(58_869, "p1"), (59_257, "p2"), (60_127, "p3"), (60_457, "p4")] {
            assert!(!r.admit(&start("startup", Some(probe), sid)).drives_row);
            assert!(!r.admit(&ev("UserPromptSubmit", Some(probe), sid)).drives_row);
            let left = r.drop_dead(&[probe]).expect("the probe was a member");
            assert_eq!(left, Left { was_main: false, drove_last: false, remaining: 1, main_change: None });
            assert!(!left.hands_over(), "no Done, no separator, nothing settled");
        }
        assert!(r.admit(&ev("Stop", Some(A), "s1")).drives_row);
    }

    #[test]
    fn a_headless_probes_whole_session_ends_without_touching_the_main() {
        // `claude -p` run from inside a session: start, prompt, Stop, end.
        let mut r = with_main();
        for e in [start("startup", Some(PROBE), "probe"), ev("UserPromptSubmit", Some(PROBE), "probe"), ev("Stop", Some(PROBE), "probe")] {
            let adm = r.admit(&e);
            assert!(!adm.drives_row && adm.main_change.is_none(), "{}", e.event);
        }
        assert_eq!(r.depart("probe", None, false), Departure::Left(Left { was_main: false, drove_last: false, remaining: 1, main_change: None }));
        assert_eq!(r.main, Some(MemberKey::Pid(A)));
    }

    #[test]
    fn a_session_keyed_stray_neither_shares_the_row_nor_inherits_it() {
        // A member whose pid the hook never resolved, killed without a
        // SessionEnd: nothing can see it leave.
        let m = Members::default();
        m.admit("dash", &ev("UserPromptSubmit", Some(A), "s1"), 100).unwrap();
        m.admit("dash", &start("startup", None, "stray"), 200).unwrap();
        assert!(m.shared_since().is_empty(), "one member a process backs");
        let left = m.drop_dead("dash", &[A], 300).unwrap();
        assert_eq!(left.main_change, Some(MainChange { from: Some(MemberKey::Pid(A)), to: None, via: Via::DepartedNoSuccessor }), "not handed to a session nothing can judge");
        assert!(left.hands_over());
        let joined = m.admit("dash", &start("startup", Some(PROBE), "p"), 400).unwrap();
        assert!(m.shared_since().is_empty(), "a new process and the stray are still one judged member");
        assert_eq!(joined.main_change, Some(MainChange { from: None, to: Some(MemberKey::Pid(PROBE)), via: Via::SoleMember }), "the one judged member is main, so drift is judged again");
        let second = m.admit("dash", &start("startup", Some(9), "q"), 500).unwrap();
        assert!(second.main_change.is_none() && !second.drives_row, "a second process does not take it");
        assert_eq!(m.shared_since().get("dash"), Some(&500), "two pid-keyed members share it");
    }

    #[test]
    fn two_pid_less_sessions_share_a_row_only_once_both_report_a_pid() {
        let m = Members::default();
        m.admit("dash", &start("startup", None, "a"), 100).unwrap();
        m.admit("dash", &start("startup", None, "b"), 200).unwrap();
        assert!(m.shared_since().is_empty());
        m.admit("dash", &ev("Stop", Some(1), "a"), 300).unwrap();
        assert!(m.shared_since().is_empty());
        m.admit("dash", &ev("Stop", Some(2), "b"), 400).unwrap();
        assert_eq!(m.shared_since().get("dash"), Some(&400));
    }

    #[test]
    fn on_a_row_that_never_had_a_pid_the_sole_member_rule_holds() {
        // A node-based install: the hook resolves no pid for anyone.
        let mut r = RowMembers::default();
        r.admit(&ev("UserPromptSubmit", None, "a"));
        r.admit(&start("startup", None, "b"));
        let Departure::Left(left) = r.depart("a", None, false) else { panic!("a was a member") };
        assert_eq!(left.main_change, Some(MainChange { from: Some(MemberKey::Session("a".into())), to: Some(MemberKey::Session("b".into())), via: Via::Succession }));
    }

    #[test]
    fn with_no_main_the_last_writer_leaving_takes_the_row_state_with_it() {
        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b")]);
        assert!(r.admit(&ev("Stop", Some(2), "b")).drives_row);
        let Departure::Left(left) = r.depart("b", None, false) else { panic!("b was a member") };
        assert!(left.drove_last && !left.was_main && left.hands_over(), "the row shows b's Stop, which nobody can answer now");
        assert_eq!(left.main_change.map(|c| c.via), Some(Via::SoleMember));

        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b")]);
        r.admit(&ev("Stop", Some(2), "b"));
        r.admit(&ev("Stop", Some(1), "a"));
        let Departure::Left(left) = r.depart("b", None, false) else { panic!("b was a member") };
        assert!(!left.drove_last && !left.hands_over(), "the row shows a's own state, which stays");
    }

    #[test]
    fn a_clear_on_a_row_with_no_main_tears_it_down_in_either_order() {
        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b")]);
        assert_eq!(r.depart("a", None, true), Departure::Clearing { drives_row: true });
        assert_eq!(r.admit(&start("clear", Some(1), "a2")).rotation, Some(Rotation { from: Some("a".into()), teardown: false }));

        let mut r = RowMembers::default();
        r.seed(&[rec(1, "a"), rec(2, "b")]);
        assert_eq!(r.admit(&start("clear", Some(1), "a2")).rotation, Some(Rotation { from: Some("a".into()), teardown: true }));
        assert_eq!(r.depart("a", None, true), Departure::Superseded);
    }

    #[test]
    fn a_clear_start_whose_key_kind_differs_from_the_main_still_belongs_to_it() {
        // Start first, with the pid reported on one event and not the other.
        let mut r = with_main();
        let adm = r.admit(&start("clear", None, "s2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: true }), "pid-less start on a pid-keyed main");
        assert_eq!(r.depart("s1", None, true), Departure::Superseded);
        assert_eq!(r.members.len(), 1);

        let mut r = RowMembers::default();
        r.admit(&ev("UserPromptSubmit", None, "s1"));
        let adm = r.admit(&start("clear", Some(A), "s2"));
        assert_eq!(adm.rotation, Some(Rotation { from: Some("s1".into()), teardown: true }), "pid-bearing start on a session-keyed main");
        assert_eq!(adm.rekeyed, Some(MemberKey::Session("s1".into())));
        assert_eq!(r.depart("s1", None, true), Departure::Superseded);
        assert_eq!(r.members.len(), 1);

        let mut r = with_main();
        assert!(r.admit(&start("clear", Some(PROBE), "s2")).joined, "another pid is another process");
    }

    #[test]
    fn a_pid_already_in_one_row_is_refused_by_another() {
        let m = Members::default();
        m.admit("y", &ev("UserPromptSubmit", Some(A), "s1"), 0).unwrap();
        assert_eq!(m.admit("z", &start("clear", Some(A), "s2"), 0), Err("y".to_string()));
        assert_eq!(m.pids("z"), Vec::<u32>::new(), "nothing changed in the refusing row");
        assert!(m.admit("y", &start("clear", Some(A), "s2"), 0).is_ok());
    }
}
