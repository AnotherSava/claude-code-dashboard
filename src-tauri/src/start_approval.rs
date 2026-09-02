//! Asking this machine's user to approve starting a session on another one.
//!
//! The awkward fact this solves: the machine that would run the session is the
//! machine nobody is at — that is *why* nothing is running there. So the prompt
//! is raised here, on the sending side, where a human is plausibly sitting,
//! because they are the one who set this agent working.
//!
//! **What crosses the wire is the answer, not the decision.** The peer still
//! resolves the directory (it supplied the candidates from its own disk), still
//! checks that the path derives the id, still checks the directory exists and
//! that Claude Code trusts it, and still refuses a sender it cannot attest. The
//! single thing it takes on faith is that a human clicked over here — which no
//! design can verify across a machine boundary, and which is why
//! `sync::post_grant` demands `Attested` rather than the fail-open `Claimed`.
//!
//! **The prompt blocks, but only briefly.** The requesting agent is awaiting a
//! loopback HTTP call, so holding it open is what makes the answer feel
//! immediate. Its patience is not ours to spend, though — it is a tool call with
//! its own timeout — so the wait is bounded and the fallback is the honest one:
//! the agent is told plainly that nothing was delivered and that the owner was
//! asked. Nothing is stored, no message body is written to disk, and the request
//! stays approvable afterwards. A grant given late is still worth having: it is
//! what stops the *next* message asking again.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::session_launcher::StartCandidate;

/// How long the requesting agent's call is held while the user decides.
///
/// Bounded by that agent's own tool timeout, not by anything here — a Bash call
/// running `curl` gives up on its own, and a handler still waiting when it does
/// is holding a connection nobody is reading. Short enough to be well inside
/// the default, long enough for a person to read the prompt and pick a folder.
pub const APPROVAL_WAIT_MS: u64 = 90_000;

/// Cap on requests waiting for an answer.
///
/// A peer that names fifty projects that do not exist must not queue fifty
/// prompts. Past the cap the newest is refused outright rather than evicting an
/// older one, so a burst cannot push a request the user was about to answer off
/// the list.
pub const MAX_PENDING: usize = 8;

/// How long a request nobody answered stays on the list.
///
/// Abandoned entries deliberately outlive their caller, but not forever: with no
/// expiry, eight ignored prompts fill [`MAX_PENDING`] permanently and every later
/// request is refused before it can even be shown — the feature goes quiet with
/// no sign of why. Long enough that a request raised while the user is out is
/// still there when they sit down.
pub const PENDING_TTL_MS: i64 = 12 * 60 * 60 * 1_000;

/// A request for permission, as the frontend renders it.
#[derive(Debug, Clone, Serialize)]
pub struct PendingStart {
    pub id: String,
    /// The device the session would start on.
    pub device: String,
    /// The project id as that device derives it.
    pub project: String,
    /// What the sending agent actually typed, echoed so the row reads the way
    /// the request was made.
    pub target: String,
    /// The agent that asked. Unverifiable by construction — the route it came in
    /// on is loopback and unauthenticated — so it is shown as a claim and never
    /// as an identity.
    pub from_agent: String,
    /// Directories the *peer* offered, never composed here.
    pub candidates: Vec<StartCandidate>,
    pub requested_at: i64,
    /// False once the requesting agent has stopped waiting. The request is still
    /// approvable — the grant is worth recording for next time — but approving
    /// it can no longer deliver the message that prompted it, and the UI must
    /// not imply otherwise.
    pub still_waiting: bool,
}

/// The user's answer: a directory to grant, or nothing.
type Answer = Option<String>;

struct Entry {
    pending: PendingStart,
    /// Present only while the requesting agent is still on the line.
    waiter: Option<oneshot::Sender<Answer>>,
}

/// Requests awaiting a human, and the callers parked on them.
#[derive(Default)]
pub struct ApprovalQueue {
    inner: Mutex<HashMap<String, Entry>>,
}

/// Why a request could not even be queued.
#[derive(Debug, PartialEq, Eq)]
pub enum QueueRefusal {
    /// One is already pending for this device and project.
    Duplicate,
    /// [`MAX_PENDING`] reached.
    Full,
}

impl ApprovalQueue {
    /// Park a request and hand back the receiver the caller waits on.
    ///
    /// Deduped on `(device, project)` rather than on the request id, because two
    /// agents asking about the same project are asking the same question and the
    /// user should answer it once. The second caller is refused rather than
    /// attached to the first: making it wait on someone else's prompt would have
    /// it block for a decision it cannot see and cannot cancel.
    pub fn enqueue(&self, pending: PendingStart) -> Result<oneshot::Receiver<Answer>, QueueRefusal> {
        let mut inner = self.inner.lock().unwrap();
        // Expire first, so a list full of yesterday's unanswered prompts cannot
        // refuse today's. Only abandoned entries expire: one with a caller still
        // parked on it is by definition current.
        inner.retain(|_, e| e.waiter.is_some() || pending.requested_at - e.pending.requested_at < PENDING_TTL_MS);
        if inner.values().any(|e| e.pending.device == pending.device && e.pending.project == pending.project) {
            return Err(QueueRefusal::Duplicate);
        }
        if inner.len() >= MAX_PENDING {
            return Err(QueueRefusal::Full);
        }
        let (tx, rx) = oneshot::channel();
        inner.insert(pending.id.clone(), Entry { pending, waiter: Some(tx) });
        Ok(rx)
    }

    /// Everything awaiting an answer, newest last.
    pub fn list(&self) -> Vec<PendingStart> {
        let mut out: Vec<PendingStart> = self.inner.lock().unwrap().values().map(|e| e.pending.clone()).collect();
        out.sort_by_key(|p| p.requested_at);
        out
    }

    /// Look up a request without removing it, so a command can act on its
    /// device and project before deciding whether it survived the answer.
    pub fn get(&self, id: &str) -> Option<PendingStart> {
        self.inner.lock().unwrap().get(id).map(|e| e.pending.clone())
    }

    /// Answer a request: `Some(dir)` approves it, `None` dismisses it.
    ///
    /// Removes it either way and wakes the parked caller if one is still there.
    /// Returns what was pending, so the caller can tell a real answer from a
    /// second click on a row that is already gone.
    pub fn resolve(&self, id: &str, answer: Answer) -> Option<PendingStart> {
        let entry = self.inner.lock().unwrap().remove(id)?;
        if let Some(waiter) = entry.waiter {
            // A closed receiver means the caller already gave up; the answer is
            // still correct, there is just no longer anyone to tell.
            let _ = waiter.send(answer);
        }
        Some(entry.pending)
    }

    /// Note that the requesting agent has stopped waiting, keeping the request
    /// on the list.
    ///
    /// The request outliving its caller is the point: the user was asked, and
    /// their answer still decides whether the *next* message has to ask again.
    /// What changes is only what an approval can achieve, which `still_waiting`
    /// tells the UI so it can say so rather than promising a delivery that can
    /// no longer happen.
    pub fn mark_abandoned(&self, id: &str) {
        if let Some(entry) = self.inner.lock().unwrap().get_mut(id) {
            entry.waiter = None;
            entry.pending.still_waiting = false;
        }
    }
}

/// Marks a request abandoned when the waiting handler goes away for **any**
/// reason.
///
/// The timeout arm is not the only way a caller stops waiting: axum drops a
/// handler's future outright when the client disconnects — an agent's `curl`
/// killed by its tool timeout, or the agent process exiting — and a plain
/// `mark_abandoned` on the timeout path never runs then. The entry would keep
/// `still_waiting: true` forever, so the prompt would go on promising a delivery
/// to a caller that is gone. `Drop` runs on cancellation too, which is the only
/// reason this is a guard rather than a line of code.
pub struct AbandonOnDrop<'a> {
    queue: &'a ApprovalQueue,
    id: String,
    answered: bool,
}

impl<'a> AbandonOnDrop<'a> {
    pub fn new(queue: &'a ApprovalQueue, id: String) -> Self {
        Self { queue, id, answered: false }
    }

    /// The request was answered, so there is nothing to abandon.
    pub fn defuse(&mut self) {
        self.answered = true;
    }
}

impl Drop for AbandonOnDrop<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.queue.mark_abandoned(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: &str, device: &str, project: &str) -> PendingStart {
        PendingStart {
            id: id.into(),
            device: device.into(),
            project: project.into(),
            target: format!("{device}/{project}"),
            from_agent: "tauri dashboard".into(),
            candidates: vec![StartCandidate { dir: "/p/x".into(), trusted: true }],
            requested_at: 1_000,
            still_waiting: true,
        }
    }

    #[test]
    fn a_request_parks_and_its_answer_wakes_the_caller() {
        let q = ApprovalQueue::default();
        let rx = q.enqueue(pending("a", "chrome", "transcripts")).unwrap();
        assert_eq!(q.list().len(), 1);
        assert!(q.resolve("a", Some("/p/transcripts".into())).is_some());
        assert_eq!(rx.blocking_recv().unwrap(), Some("/p/transcripts".to_string()));
        assert!(q.list().is_empty(), "an answered request leaves the list");
    }

    #[test]
    fn a_dismissal_wakes_the_caller_with_nothing() {
        let q = ApprovalQueue::default();
        let rx = q.enqueue(pending("a", "chrome", "transcripts")).unwrap();
        q.resolve("a", None);
        assert_eq!(rx.blocking_recv().unwrap(), None);
    }

    /// Two agents asking about one project are asking one question.
    #[test]
    fn the_same_project_is_only_asked_about_once() {
        let q = ApprovalQueue::default();
        let _rx = q.enqueue(pending("a", "chrome", "transcripts")).unwrap();
        assert_eq!(q.enqueue(pending("b", "chrome", "transcripts")).unwrap_err(), QueueRefusal::Duplicate);
        assert!(q.enqueue(pending("c", "chrome", "scheduler")).is_ok(), "a different project is a different question");
        assert!(q.enqueue(pending("d", "air", "transcripts")).is_ok(), "so is the same project on another machine");
    }

    /// A burst must not be able to push aside a request the user was about to
    /// answer, so the cap refuses the newest rather than evicting the oldest.
    #[test]
    fn a_burst_is_capped_without_losing_the_oldest() {
        let q = ApprovalQueue::default();
        let mut keep = Vec::new();
        for i in 0..MAX_PENDING {
            keep.push(q.enqueue(pending(&format!("id{i}"), "chrome", &format!("p{i}"))).unwrap());
        }
        assert_eq!(q.enqueue(pending("over", "chrome", "extra")).unwrap_err(), QueueRefusal::Full);
        assert!(q.get("id0").is_some(), "the first request is still there to be answered");
    }

    /// The caller giving up must not take the question away from the user.
    #[test]
    fn an_abandoned_request_stays_approvable() {
        let q = ApprovalQueue::default();
        let rx = q.enqueue(pending("a", "chrome", "transcripts")).unwrap();
        drop(rx);
        q.mark_abandoned("a");
        let still = q.get("a").expect("the request outlives the caller");
        assert!(!still.still_waiting, "and says plainly that approving it can no longer deliver the message");
        assert!(q.resolve("a", Some("/p/transcripts".into())).is_some(), "the grant is still worth recording for next time");
    }

    /// Cancellation, not just timeout, must mark the request abandoned — axum
    /// drops the handler's future when the client disconnects, and a row that
    /// went on claiming a caller was waiting would promise a delivery to nobody.
    #[test]
    fn a_dropped_waiter_marks_the_request_abandoned() {
        let q = ApprovalQueue::default();
        let _rx = q.enqueue(pending("a", "chrome", "transcripts")).unwrap();
        {
            let _guard = AbandonOnDrop::new(&q, "a".into());
        }
        assert!(!q.get("a").unwrap().still_waiting);

        let _rx2 = q.enqueue(pending("b", "chrome", "scheduler")).unwrap();
        {
            let mut guard = AbandonOnDrop::new(&q, "b".into());
            guard.defuse();
        }
        assert!(q.get("b").unwrap().still_waiting, "an answered request is left alone");
    }

    /// Without an expiry, eight prompts nobody answered fill the cap forever and
    /// every later request is refused before it can be shown.
    #[test]
    fn abandoned_requests_expire_but_live_ones_never_do() {
        let q = ApprovalQueue::default();
        let mut old = pending("old", "chrome", "p0");
        old.requested_at = 0;
        let _rx = q.enqueue(old).unwrap();
        q.mark_abandoned("old");

        let live = pending("live", "chrome", "p1");
        let _rx2 = q.enqueue(live).unwrap();

        let mut later = pending("later", "chrome", "p2");
        later.requested_at = PENDING_TTL_MS + 1;
        q.enqueue(later).unwrap();
        assert!(q.get("old").is_none(), "an unanswered prompt from yesterday makes way");
        assert!(q.get("live").is_some(), "one with a caller still parked on it does not");
    }

    #[test]
    fn answering_an_unknown_request_changes_nothing() {
        assert!(ApprovalQueue::default().resolve("nope", None).is_none());
    }
}
