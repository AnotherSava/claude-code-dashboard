//! What a row shows for a task another agent began.
//!
//! Claude Code delivers a message from another agent as an ordinary
//! `UserPromptSubmit`, so the row's prompt is the whole wire envelope: Claude
//! Code's own `<cross-session-message from="uds:…" from-name="…">` around a
//! `SendMessage` between two sessions on this machine, or this dashboard's own
//! relay envelope around a message from another machine
//! (`peer_message::build_content`). Shown as it arrives, the row's task line and
//! the terminal's context line open on 120 characters of routing.
//!
//! What the row shows instead is the **sender's own task** — what its user asked
//! it to do, which is the reason the message exists — with no marker and no
//! sender name. It is resolved once, when the prompt arrives, and stored on the
//! row as `AgentSession::delegated_task`, so the sender moving on to other work
//! later does not rewrite what this row says it was asked. `original_prompt`
//! keeps the envelope; nothing about what is stored for the history changes.
//!
//! Where the sender's task cannot be had — the sending session has gone, the
//! registry cannot be read, two live sessions share the name, the relay carried
//! no `from_task` — the row
//! shows the first line of the message itself, the next-truest account of what
//! this session was asked, stored as `AgentSession::message_line`. Never the
//! envelope. The two live in separate fields because they are different facts:
//! a line another agent wrote is not a task anybody gave, and the chain below
//! must never pass one off as the next row's task.
//!
//! A chain of agents is followed through those snapshots rather than walked at
//! arrival: where the sender's own task was another agent's message, its row
//! already holds the `delegated_task` settled when that message arrived, and
//! that is what this row takes. A sender whose own arrival settled nothing ends
//! the chain, since the envelope its row kept can be days old and resolving it
//! now would ask today's registry about yesterday's sender (see [`resolve`]).
//!
//! A relayed message's task is the sending dashboard's record of it, read off the
//! envelope header, which `header_safe` wrote for the receiving model — reserved
//! words `[redacted]`, quotes dropped, 200 characters — and the row shows that
//! copy.
//!
//! Every resolved task is cut to the same excerpt a message line is, since a
//! person's prompt can run to thousands of characters and a row shows one line.

use std::borrow::Cow;

use crate::adapters::claude::clean_prompt;
use crate::peer_message::{header_task_names_a_message, parse_agent_message, parse_harness_prompt, AgentMessage};
use crate::session_registry::{SenderLookup, SessionRegistry};
use crate::state::AgentSession;

/// Longest message line or resolved task a row shows, in characters, the
/// ellipsis included. A line from the raw prompt is usually far shorter; the cap
/// is for the envelopes `clean_prompt` collapsed onto one line, whose "first
/// line" is the whole message, and for a long prompt resolved as a task. Every `label` an agent message sets is one — read by
/// `AgentSession::primary_text` for a row with no task — as is an `original_prompt`
/// persisted before `message_line` existed or synced from a peer predating it.
/// [`shown`] reads all of them, which is why the cut is needed.
const EXCERPT_CHARS: usize = 200;

/// How an arriving message's text was settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The sender's row held its task: a person's prompt there, or a task
    /// resolved for it when its own message arrived.
    ResolvedLocal,
    /// The relay envelope's record of its sender's task.
    ResolvedRelay,
    /// Neither: the arriving message's own first line stands in, for the
    /// reason given.
    BodyFallback(Miss),
}

impl Outcome {
    /// The `outcome` value on the `prompt_origin` log line.
    pub fn slug(self) -> &'static str {
        match self {
            Outcome::ResolvedLocal => "resolved_local",
            Outcome::ResolvedRelay => "resolved_relay",
            Outcome::BodyFallback(_) => "body_fallback",
        }
    }
}

/// Why the sender's task could not be had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    /// The session registry could not be read, so no sender could be looked up.
    RegistryUnreadable,
    /// No live session publishes the envelope's inbox, or, for an envelope
    /// naming no inbox, carries its name.
    SenderNotLive,
    /// The envelope names no inbox and several live sessions carry its name.
    SenderAmbiguous,
    /// The sender is live but this dashboard holds no row for it.
    SenderHasNoRow,
    /// The sender's row has no task recorded.
    SenderHasNoTask,
    /// The sender's own task was another agent's message whose sender was not
    /// resolved when it arrived — its row keeps the envelope, with a
    /// `message_line` or, persisted before resolution existed, with neither.
    /// Not looked up now: see [`sender_task`].
    SenderTaskUnresolved,
    /// The relay envelope carries no `from_task` line.
    RelayCarriedNoTask,
    /// The relay's `from_task` is itself an envelope. It names a session on the
    /// *sending* machine, which this machine's registry cannot answer for.
    RelayTaskIsMessage,
}

impl Miss {
    /// The `why` value on the `prompt_origin` log line.
    pub fn slug(self) -> &'static str {
        match self {
            Miss::RegistryUnreadable => "registry_unreadable",
            Miss::SenderNotLive => "sender_not_live",
            Miss::SenderAmbiguous => "sender_ambiguous",
            Miss::SenderHasNoRow => "sender_has_no_row",
            Miss::SenderHasNoTask => "sender_has_no_task",
            Miss::SenderTaskUnresolved => "sender_task_unresolved",
            Miss::RelayCarriedNoTask => "relay_carried_no_task",
            Miss::RelayTaskIsMessage => "relay_task_is_message",
        }
    }
}

/// The settled text for one arriving message, and how it was reached.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    /// What the row shows: the resolved task, or for a [`Outcome::BodyFallback`]
    /// the message's first line. `None` only where the sender's task could not
    /// be had and the message has no text either.
    pub text: Option<String>,
    pub outcome: Outcome,
    /// The sender's local row, where the lookup reached one, for the log.
    pub sender: Option<String>,
}

impl Resolution {
    /// The text as `AgentSession::delegated_task`: set only when it is a task.
    pub fn delegated_task(&self) -> Option<String> {
        self.text.clone().filter(|_| !matches!(self.outcome, Outcome::BodyFallback(_)))
    }

    /// The text as `AgentSession::message_line`: set only when it is the
    /// message's own line standing in for a task that could not be had.
    pub fn message_line(&self) -> Option<String> {
        self.text.clone().filter(|_| matches!(self.outcome, Outcome::BodyFallback(_)))
    }
}

/// Settle an arriving message's text: its sender's own task, else the
/// message's first line.
///
/// The questions it asks of the live system are closures, so the rule is
/// tested without one: `sender_row` names the row of the local session an
/// envelope's inbox or name identifies, and `row_task` reads the task that row
/// hands onward ([`sender_task`]).
///
/// A relay's task is the header's `header_safe` copy, judged by
/// [`header_task_names_a_message`], which reads an envelope there with its
/// quotes stripped and its end cut off.
pub fn resolve(msg: &AgentMessage, sender_row: &dyn Fn(Option<&str>, Option<&str>) -> Result<String, Miss>, row_task: &dyn Fn(&str) -> Result<String, Miss>) -> Resolution {
    let fallback = |miss: Miss, sender: Option<String>| Resolution { text: first_line(msg.body()), outcome: Outcome::BodyFallback(miss), sender };
    let nonblank = |t: &String| !t.trim().is_empty();
    let (task, outcome, sender) = match msg {
        AgentMessage::Relayed { task, .. } => match task.clone().filter(nonblank) {
            None => return fallback(Miss::RelayCarriedNoTask, None),
            Some(task) if header_task_names_a_message(&task) => return fallback(Miss::RelayTaskIsMessage, None),
            Some(task) => (task, Outcome::ResolvedRelay, None),
        },
        AgentMessage::Local { inbox, name, .. } => {
            let row = match sender_row(inbox.as_deref(), name.as_deref()) {
                Ok(row) => row,
                Err(miss) => return fallback(miss, None),
            };
            match row_task(&row) {
                Ok(task) => (task, Outcome::ResolvedLocal, Some(row)),
                Err(miss) => return fallback(miss, Some(row)),
            }
        }
    };
    Resolution { text: Some(clean_prompt(&task)).filter(|t| !t.is_empty()).map(excerpt), outcome, sender }
}

/// `text` as a row may show it: itself, unless it is an agent message's
/// envelope, which shows as the first line of the message inside it, cut to
/// [`EXCERPT_CHARS`] — `None` when that is empty. An envelope a row stored as
/// `original_prompt` was collapsed onto one line by `clean_prompt`, so for that
/// copy the "first line" is the whole message, and the cut is what keeps it an
/// excerpt.
///
/// The guarantee behind every display surface that no envelope reaches the
/// screen, including the texts `delegated_task` and `message_line` do not cover:
/// a row's past task read back out of its dialog, a prompt persisted before
/// they existed, and a `label` set by a message that arrived while no task was
/// recorded.
///
/// A prompt Claude Code submitted on its own account (`parse_harness_prompt`)
/// is no task and has no line to stand in for one, so it shows as `None`.
pub fn shown(text: &str) -> Option<Cow<'_, str>> {
    if parse_harness_prompt(text).is_some() {
        return None;
    }
    match parse_agent_message(text) {
        Some(msg) => first_line(msg.body()).map(Cow::Owned),
        None => Some(Cow::Borrowed(text)),
    }
}

/// The first line of `text` that has anything on it, normalised as a typed
/// prompt is and cut to [`EXCERPT_CHARS`].
fn first_line(text: &str) -> Option<String> {
    text.lines().map(clean_prompt).find(|l| !l.is_empty()).map(excerpt)
}

/// `line` cut to [`EXCERPT_CHARS`], the ellipsis included.
fn excerpt(line: String) -> String {
    if line.chars().count() > EXCERPT_CHARS {
        format!("{}…", line.chars().take(EXCERPT_CHARS - 1).collect::<String>().trim_end())
    } else {
        line
    }
}

/// What [`for_arrival`] asks of the running dashboard.
pub struct Live<'a> {
    /// The raw `AppState` snapshot, before this prompt is applied. A sender is a
    /// *local* row, so a synced row of the same id is skipped, and its task is
    /// the one recorded on this machine.
    pub rows: &'a [AgentSession],
    pub registry: Option<&'a SessionRegistry>,
    /// `ChatIdRegistry::anchored`, so a sender that has `cd`-ed resolves to the
    /// row it actually writes to, the way the roster and the restore resolve one.
    pub anchored: &'a dyn Fn(&str) -> Option<String>,
    pub projects_root: Option<&'a str>,
    pub now: i64,
}

/// Settle the text for a message arriving now, against the live registry and
/// the rows as they stand before this prompt is applied. The caller logs it with
/// [`log`] once the event has been applied, since only then is it known whether
/// the row took it.
pub fn for_arrival(msg: &AgentMessage, live: &Live) -> Resolution {
    let sender_row = |inbox: Option<&str>, name: Option<&str>| match live.registry.map(|r| r.sender_of(inbox, name, live.anchored, live.projects_root, live.now)) {
        Some(SenderLookup::Found(row)) => Ok(row),
        Some(SenderLookup::Ambiguous) => Err(Miss::SenderAmbiguous),
        Some(SenderLookup::NotFound) => Err(Miss::SenderNotLive),
        Some(SenderLookup::Unreadable) | None => Err(Miss::RegistryUnreadable),
    };
    resolve(msg, &sender_row, &|row| sender_task(live.rows, row))
}

/// The task the local row `row` hands onward when its agent sends a message:
/// its [`AgentSession::person_task`], the same task the relay reports for it
/// (`http_server::sender_task`).
///
/// A sender whose task is itself an agent message's envelope ends the chain
/// with [`Miss::SenderTaskUnresolved`] rather than being looked up in turn.
/// Where the sender's own message was resolved, its row's task is the
/// `delegated_task` settled then, so a resolved chain never reaches this. What
/// does reach it is an envelope whose lookup failed when it arrived, or one
/// persisted before lookups existed, and either may be days old: on macOS its
/// inbox is pid-derived and may now name an unrelated session, its name may now
/// belong to a later session in the same project, and a refusal made then would
/// be decided again under different conditions. Each would report a session
/// that is not the sender, or the sender's task as it is now rather than as it
/// was.
fn sender_task(rows: &[AgentSession], row: &str) -> Result<String, Miss> {
    let s = rows.iter().find(|s| s.origin.is_none() && s.id == row).ok_or(Miss::SenderHasNoRow)?;
    match s.person_task().map(str::trim).filter(|t| !t.is_empty()) {
        Some(task) => Ok(task.to_string()),
        None if s.original_prompt.as_deref().is_some_and(|p| parse_agent_message(p).is_some()) => Err(Miss::SenderTaskUnresolved),
        None => Err(Miss::SenderHasNoTask),
    }
}

/// Whether the row, after the event that carried `prompt` was applied, took
/// `res` as its task's text. The prompt becomes the row's task only on a task
/// boundary (`label_policy::select`); a message answering a question, one
/// arriving while the row is blocked or its turn is still running, or a relayed
/// reply to a message this row's agent sent, leaves the previous task in place.
pub fn adopted(row: Option<&AgentSession>, prompt: Option<&str>, res: &Resolution) -> bool {
    row.is_some_and(|s| s.original_prompt.as_deref() == prompt && s.delegated_task == res.delegated_task() && s.message_line == res.message_line())
}

/// Log how an arriving message's text was settled (`decision = "prompt_origin"`),
/// once per arrival, after the event is applied, with [`adopted`]'s answer, so a
/// resolution the row left unused is never recorded as the row's text.
pub fn log(chat_id: &str, msg: &AgentMessage, res: &Resolution, adopted: bool) {
    let transport = match msg {
        AgentMessage::Local { .. } => "local",
        AgentMessage::Relayed { .. } => "relay",
    };
    let why = match res.outcome {
        Outcome::BodyFallback(miss) => Some(miss.slug()),
        Outcome::ResolvedLocal | Outcome::ResolvedRelay => None,
    };
    // The shown text itself stays out of the line: a resolved one is a prompt
    // its own row's `classify` line already recorded, and a fallback is the
    // message body, which nothing logs beyond that same line.
    tracing::debug!(
        chat_id = %chat_id,
        decision = "prompt_origin",
        transport,
        outcome = res.outcome.slug(),
        why,
        sender = ?res.sender,
        shown_chars = res.text.as_deref().map_or(0, |t| t.chars().count()),
        adopted,
        "agent message: row text settled"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_message::{build_content, Relayed};

    /// A real `SendMessage` envelope's shape as it sits in `prompt_history.json`,
    /// the body cut down to a placeholder.
    fn local(inbox: &str, name: &str, body: &str) -> String {
        format!("<cross-session-message from=\"uds:{inbox}\" from-name=\"{name}\" from-mode=\"prompting\">\n{body}\n</cross-session-message>")
    }

    /// A relay envelope exactly as `build_content` writes it.
    fn relayed(from_task: Option<&str>, text: &str) -> String {
        build_content(&Relayed { origin_device: "air", from_agent: "x", from_label: None, from_task, text, reply_to: None, message_id: "air-1-0", in_reply_to: None, reply_port: 9077, attestation: crate::tailnet::Attestation::Claimed, tailnet_user: None })
    }

    fn msg(text: &str) -> AgentMessage {
        parse_agent_message(text).expect("an envelope")
    }

    /// Rows by inbox: each entry is (inbox, row id, that row's task text).
    fn world<'a>(rows: &'a [(&'a str, &'a str, &'a str)]) -> (impl Fn(Option<&str>, Option<&str>) -> Result<String, Miss> + 'a, impl Fn(&str) -> Result<String, Miss> + 'a) {
        let sender_row = move |inbox: Option<&str>, _name: Option<&str>| rows.iter().find(|(i, _, _)| Some(*i) == inbox).map(|(_, r, _)| r.to_string()).ok_or(Miss::SenderNotLive);
        let row_task = move |row: &str| rows.iter().find(|(_, r, _)| *r == row).map(|(_, _, t)| t.to_string()).ok_or(Miss::SenderHasNoRow);
        (sender_row, row_task)
    }

    #[test]
    fn a_local_sender_s_own_prompt_is_what_the_row_shows() {
        let (s, t) = world(&[("pipe-a", "agwinterm", "add a title-bar caption to agwinterm")]);
        let res = resolve(&msg(&local("pipe-a", "agwinterm-x", "placeholder body")), &s, &t);
        assert_eq!(res, Resolution { text: Some("add a title-bar caption to agwinterm".into()), outcome: Outcome::ResolvedLocal, sender: Some("agwinterm".into()) });
        assert_eq!((res.delegated_task().as_deref(), res.message_line()), (Some("add a title-bar caption to agwinterm"), None));
    }

    #[test]
    fn a_relay_s_from_task_is_what_the_row_shows() {
        let (s, t) = world(&[]);
        let res = resolve(&msg(&relayed(Some("fix the parser"), "placeholder body")), &s, &t);
        assert_eq!((res.text.as_deref(), res.outcome), (Some("fix the parser"), Outcome::ResolvedRelay));
    }

    /// The header's copy is redacted for the receiving model, and shown on one
    /// line like a typed prompt.
    #[test]
    fn a_relay_shows_the_header_s_copy_of_its_task() {
        let envelope = relayed(Some("fix the /api/message receipt\nand make sure it's verified"), "placeholder body");
        let (s, t) = world(&[]);
        assert_eq!(resolve(&msg(&envelope), &s, &t).text.as_deref(), Some("fix the [redacted] receipt and make sure it's [redacted]"));
    }

    #[test]
    fn a_resolved_task_is_cut_to_the_excerpt_a_message_line_is() {
        let long = format!("start {}", "word ".repeat(1_000));
        let table = [("pipe-a", "a", long.as_str())];
        let (s, t) = world(&table);
        let text = resolve(&msg(&local("pipe-a", "a", "placeholder")), &s, &t).text.expect("a task");
        assert_eq!(text.chars().count(), EXCERPT_CHARS);
        assert!(text.starts_with("start word") && text.ends_with('…'), "{text}");
    }

    /// A chain of agents reaches the person through the snapshot each row took
    /// when its own message arrived: B's task is the `delegated_task` settled
    /// then ([`sender_task`]), which is A's prompt.
    #[test]
    fn a_chain_reaches_the_person_through_the_sender_s_own_snapshot() {
        let table = [("pipe-b", "b", "rename the tray item")];
        let (s, t) = world(&table);
        let res = resolve(&msg(&local("pipe-b", "b", "placeholder")), &s, &t);
        assert_eq!(res, Resolution { text: Some("rename the tray item".into()), outcome: Outcome::ResolvedLocal, sender: Some("b".into()) });
    }

    /// B's lookup of its own sender failed when that message arrived, so B's
    /// row kept the envelope. Days later its inbox, pid-derived on macOS, names
    /// an unrelated live session; following it would report that session's task
    /// as the person behind C's message. The chain ends at B instead.
    #[test]
    fn a_sender_whose_own_message_was_never_resolved_ends_the_chain() {
        use crate::state::{AppState, SetInput, Status};
        // B's row as its own unresolved message left it: the envelope as its
        // task, with no `delegated_task`.
        let rows_with_task = |task: &str| {
            let state = AppState::new();
            state.apply_set(SetInput { id: "b".into(), status: Status::Working, label: Some(clean_prompt(task)), source: None, model: None, input_tokens: None, dialog_entry: None, waiting_backstop_armed: false, turn_from_relay: None, delegated_task: None, message_line: None, message_is_reply: None }, 1_000, &[], None);
            state.snapshot()
        };
        let found = |_: Option<&str>, _: Option<&str>| Ok("b".to_string());

        let rows = rows_with_task(&local("/tmp/cc-socks/95256.sock", "tripit", "Please commit your two files"));
        let res = resolve(&msg(&local("pipe-b", "b", "done, see the diff")), &found, &|row| sender_task(&rows, row));
        assert_eq!(res, Resolution { text: Some("done, see the diff".into()), outcome: Outcome::BodyFallback(Miss::SenderTaskUnresolved), sender: Some("b".into()) });
        assert_eq!((res.delegated_task(), res.message_line().as_deref()), (None, Some("done, see the diff")), "a fallback is a message line, never a task");

        let rows = rows_with_task(&relayed(Some("re-shoot the macOS figures"), "please look at this"));
        assert_eq!(resolve(&msg(&local("pipe-b", "b", "placeholder")), &found, &|row| sender_task(&rows, row)).outcome, Outcome::BodyFallback(Miss::SenderTaskUnresolved), "a relay envelope kept without a task is no different");

        let rows = rows_with_task("fix the parser");
        assert_eq!(sender_task(&rows, "b").as_deref(), Ok("fix the parser"), "a typed task is handed onward");
        assert_eq!(sender_task(&rows, "c"), Err(Miss::SenderHasNoRow));
    }

    #[test]
    fn every_miss_shows_the_message_s_first_line_never_the_envelope() {
        let gone = |_: Option<&str>, _: Option<&str>| Err(Miss::SenderNotLive);
        let unread = |_: &str| -> Result<String, Miss> { unreachable!("no sender, no row read") };
        let res = resolve(&msg(&local("pipe-x", "x", "\n  please   pull the repo \nmore")), &gone, &unread);
        assert_eq!((res.text.as_deref(), res.outcome, res.sender), (Some("please pull the repo"), Outcome::BodyFallback(Miss::SenderNotLive), None));

        let no_task = |_: &str| Err(Miss::SenderHasNoTask);
        let found = |_: Option<&str>, _: Option<&str>| Ok("b".to_string());
        let res = resolve(&msg(&local("pipe-b", "b", "body")), &found, &no_task);
        assert_eq!((res.outcome, res.sender.as_deref()), (Outcome::BodyFallback(Miss::SenderHasNoTask), Some("b")));

        assert_eq!(resolve(&msg(&relayed(None, "the body")), &gone, &unread).outcome, Outcome::BodyFallback(Miss::RelayCarriedNoTask));

        // A relay whose sending row's task was a `SendMessage` on that machine,
        // sent by a peer that reports its row's raw prompt: `header_safe` strips
        // the quotes on the way into the header, and the copy must still be
        // known for an envelope rather than shown as a task.
        let nested = relayed(Some(&clean_prompt(&local("/tmp/cc-socks/95256.sock", "far", "x"))), "the body");
        let res = resolve(&msg(&nested), &gone, &unread);
        assert_eq!((res.text.as_deref(), res.outcome), (Some("the body"), Outcome::BodyFallback(Miss::RelayTaskIsMessage)), "the sending machine's inbox is never looked up here");

        // Glyphs a person's prompt would lose lose here too: one normaliser.
        assert_eq!(resolve(&msg(&local("pipe-x", "x", "⎿ Error: │ build failed")), &gone, &unread).text.as_deref(), Some("Error: build failed"));

        assert_eq!(resolve(&msg(&local("pipe-x", "x", "  ")), &gone, &unread).text, None, "an empty message has nothing to stand in");
    }

    /// A message answering a question is resolved like any other, and the row
    /// keeps its previous task; only one that starts a task is adopted, which is
    /// what the log line reports.
    #[test]
    fn only_a_message_that_starts_a_task_is_adopted() {
        use crate::state::{AppState, SetInput, Status};
        let envelope = clean_prompt(&local("pipe-a", "a", "here is the answer"));
        let res = Resolution { text: Some("add a title-bar caption".into()), outcome: Outcome::ResolvedLocal, sender: Some("a".into()) };
        let set = |status, label: Option<&str>, res: Option<&Resolution>| SetInput { id: "r".into(), status, label: label.map(str::to_string), source: None, model: None, input_tokens: None, dialog_entry: None, waiting_backstop_armed: false, turn_from_relay: None, delegated_task: res.and_then(Resolution::delegated_task), message_line: res.and_then(Resolution::message_line), message_is_reply: label.and_then(parse_agent_message).map(|m| m.is_reply()) };
        let row = |state: &AppState| state.snapshot().into_iter().find(|s| s.id == "r");

        let state = AppState::new();
        state.apply_set(set(Status::Working, Some(&envelope), Some(&res)), 1_000, &[], None);
        assert!(adopted(row(&state).as_ref(), Some(&envelope), &res), "a message that starts the row's task");

        let state = AppState::new();
        state.apply_set(set(Status::Working, Some("fix the parser"), None), 1_000, &[], None);
        state.apply_set(set(Status::Blocked, Some("has a question"), None), 2_000, &[], None);
        state.apply_set(set(Status::Working, Some(&envelope), Some(&res)), 3_000, &[], None);
        assert!(!adopted(row(&state).as_ref(), Some(&envelope), &res), "an answer to a question is not a new task");
        assert!(!adopted(None, Some(&envelope), &res), "no row, nothing adopted");
    }

    #[test]
    fn shown_leaves_a_person_s_text_alone_and_unwraps_an_envelope() {
        assert_eq!(shown("fix the build").as_deref(), Some("fix the build"));
        assert_eq!(shown("about <cross-session-message from=\"uds:x\"> tags").as_deref(), Some("about <cross-session-message from=\"uds:x\"> tags"), "a mention is not an envelope");
        assert_eq!(shown(&local("pipe", "n", "the message\nmore")).as_deref(), Some("the message"));
        assert_eq!(shown(&local("pipe", "n", "")), None);
    }

    /// The copy a row stored as `original_prompt` has no lines left, so its
    /// first line is the whole message; it is cut to an excerpt.
    #[test]
    fn a_collapsed_envelope_shows_an_excerpt_not_its_whole_message() {
        let long = format!("start {}", "word ".repeat(400));
        let shown = shown(&clean_prompt(&local("pipe", "n", &long))).expect("text").into_owned();
        assert_eq!(shown.chars().count(), EXCERPT_CHARS);
        assert!(shown.starts_with("start word") && shown.ends_with('…'), "{shown}");
    }
}
