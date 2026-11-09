//! Typing an allowlisted built-in command into a local session, for
//! `POST /api/session/command`.
//!
//! A coordinating agent can message another session but cannot make it run
//! `/clear`: a built-in command is the user's to type, not something the
//! receiving agent can invoke. Typing it into the session's composer is the
//! only route, and the dashboard already owns the safe version of that typing,
//! the one `idle_close` uses to type `/exit`
//! ([`crate::terminals::composer::type_command`]). This module decides which
//! session the keys go to, and why none, as pure functions over the dashboard's
//! rows and the terminal's sessions; the HTTP handler in `http_server` reads
//! the state, asks the adapter and logs.
//!
//! The session is found the way `idle_close` finds the one it closes: the row
//! named by the request, then the terminal session whose title the label join
//! gives that row ([`labels::session_of`]). A row on another machine is
//! refused rather than relayed, since typing is done by the dashboard whose
//! terminal holds the session, and a row whose main agent is mid-turn is
//! refused because a command typed then is queued behind the turn rather than
//! run. Mid-turn is read through a subagent's permission prompt, which shows
//! the row BLOCK while the main agent works on, and read again just before the
//! keys go, since a prompt can start a turn while the terminal is being asked.

use crate::state::{AgentSession, Status};
use crate::terminals::composer::Command;
use crate::terminals::labels::{self, DesiredLabel};
use crate::terminals::CloseTarget;

/// What `POST /api/session/command` is told. Exactly one of `project` and
/// `session_id` names the session.
#[derive(Debug, serde::Deserialize)]
pub struct CommandRequest {
    /// A row id on this machine, as `/api/agents` lists it.
    #[serde(default)]
    pub project: Option<String>,
    /// A Claude Code session id, resolved to the row it is anchored to.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `/clear` or `/exit`.
    #[serde(default)]
    pub command: String,
}

/// Why nothing was typed. Each is a stable `reason` slug on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The command is not on the allowlist.
    UnknownCommand,
    /// Neither or both of `project` and `session_id` were given.
    NoTarget,
    /// The project is a row synced from another machine.
    NotLocal,
    /// No local row by that id, or no row the session id is anchored to.
    NoSuchSession,
    /// The row's main agent is `Working`.
    Busy,
    /// No terminal on this machine can type into a session.
    Unsupported,
    /// The terminal could not be asked for its sessions.
    TerminalUnreadable,
    /// The join found no session, or refused the row: `no_session`,
    /// `same_label` or `shared_title`.
    Join(&'static str),
    /// The adapter refused, with its sentence: a draft in the composer, no
    /// composer on screen, the read-back failing, the session in front.
    Typing(String),
}

impl Refusal {
    pub fn slug(&self) -> &'static str {
        match self {
            Refusal::UnknownCommand => "unknown_command",
            Refusal::NoTarget => "no_target",
            Refusal::NotLocal => "not_local",
            Refusal::NoSuchSession => "no_such_session",
            Refusal::Busy => "busy",
            Refusal::Unsupported => "unsupported",
            Refusal::TerminalUnreadable => "terminal_unreadable",
            Refusal::Join(slug) => slug,
            Refusal::Typing(_) => "typing_refused",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Refusal::UnknownCommand => "only /clear and /exit can be typed".to_string(),
            Refusal::NoTarget => "name the session by exactly one of project and session_id".to_string(),
            Refusal::NotLocal => "the session runs on another machine; ask that machine's dashboard".to_string(),
            Refusal::NoSuchSession => "no session on this machine by that name".to_string(),
            Refusal::Busy => "the session is mid-turn, and a command typed now would wait behind the turn".to_string(),
            Refusal::Unsupported => "no terminal on this machine can type into a session".to_string(),
            Refusal::TerminalUnreadable => "the terminal could not be asked for its sessions".to_string(),
            Refusal::Join(slug) => labels::join_refusal(slug).to_string(),
            Refusal::Typing(why) => why.clone(),
        }
    }

    /// `400` for a request that could never succeed as written, `409` for one
    /// refused by the state it met.
    pub fn is_bad_request(&self) -> bool {
        matches!(self, Refusal::UnknownCommand | Refusal::NoTarget)
    }
}

/// The command and the local row a request names, or why it names none.
/// `rows` is every row the dashboard shows, synced ones included, so a
/// project on another machine is told so rather than reported missing.
/// `anchored` resolves a session id to its row. A refusal carries the row
/// the request named, empty where it named none, for the answer's `row`.
pub fn resolve(req: &CommandRequest, rows: &[AgentSession], anchored: impl Fn(&str) -> Option<String>) -> Result<(Command, String), (String, Refusal)> {
    let command = Command::parse(&req.command).ok_or((String::new(), Refusal::UnknownCommand))?;
    let nonblank = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let row = match (nonblank(&req.project), nonblank(&req.session_id)) {
        (Some(project), None) => project,
        (None, Some(sid)) => anchored(&sid).ok_or((String::new(), Refusal::NoSuchSession))?,
        _ => return Err((String::new(), Refusal::NoTarget)),
    };
    let refused = |r: Refusal| (row.clone(), r);
    let session = rows.iter().find(|s| s.id == row).ok_or_else(|| refused(Refusal::NoSuchSession))?;
    if session.origin.is_some() {
        return Err(refused(Refusal::NotLocal));
    }
    still_free(Some(session.base_status())).map_err(refused)?;
    Ok((command, row))
}

/// Whether a row whose main agent's status is `base` (`None` for a row that
/// has gone) may be typed into: what [`resolve`] checks on the snapshot and
/// the handler checks again just before the keys go.
pub fn still_free(base: Option<Status>) -> Result<(), Refusal> {
    match base {
        None => Err(Refusal::NoSuchSession),
        Some(Status::Working) => Err(Refusal::Busy),
        Some(_) => Ok(()),
    }
}

/// The key of the terminal session the label join gives `row`, among
/// `targets`, joined against every local row in `rows`.
pub fn target_key(rows: &[AgentSession], targets: &[CloseTarget], row: &str) -> Result<String, Refusal> {
    let labels: Vec<DesiredLabel> = rows.iter().filter(|s| s.origin.is_none()).map(|s| DesiredLabel::for_row(s, None)).collect();
    labels::session_of(&labels, targets, row).map(|t| t.key.clone()).map_err(Refusal::Join)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: Status) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            status,
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
            origin: None,
            instruction_drift: false,
            canary: crate::state::Canary::Off,
            attended_at: None,
            content_seen_at: None,
            origin_label: None,
            relay_exchange_from: None,
            held_question: None,
            delegated_task: None,
            message_line: None,
            clean_claim_at: None,
            read: false,
            name_shared_by: None,
            row_line: None,
            task_lines: Vec::new(),
            dialog_messages: Vec::new(),
            subagent_gate: None,
            terminal_stale_at: None,
        }
    }

    fn req(project: Option<&str>, session_id: Option<&str>, command: &str) -> CommandRequest {
        CommandRequest { project: project.map(Into::into), session_id: session_id.map(Into::into), command: command.into() }
    }

    /// [`resolve`] with the row a refusal carries dropped.
    fn res(req: &CommandRequest, rows: &[AgentSession], anchored: impl Fn(&str) -> Option<String>) -> Result<(Command, String), Refusal> {
        resolve(req, rows, anchored).map_err(|(_, r)| r)
    }

    #[test]
    fn a_refusal_carries_the_row_the_request_named() {
        let named = |project: &str, rows: &[AgentSession]| resolve(&req(Some(project), None, "/clear"), rows, none).unwrap_err().0;
        assert_eq!(named("web", &[row("web", Status::Working)]), "web");
        assert_eq!(named("api", &[row("web", Status::Done)]), "api");
        assert_eq!(resolve(&req(Some("web"), None, "/rm"), &[row("web", Status::Working)], none).unwrap_err().0, "");
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_command_off_the_allowlist_is_refused_before_anything_is_looked_up() {
        let rows = [row("web", Status::Done)];
        for command in ["/compact", "hello", "", "/clear please"] {
            let err = res(&req(Some("web"), None, command), &rows, none).unwrap_err();
            assert_eq!(err, Refusal::UnknownCommand, "{command:?}");
            assert!(err.is_bad_request());
        }
        assert_eq!(res(&req(Some("nowhere"), None, "/rm"), &rows, none), Err(Refusal::UnknownCommand));
    }

    #[test]
    fn exactly_one_name_is_required() {
        let rows = [row("web", Status::Done)];
        assert_eq!(res(&req(None, None, "/clear"), &rows, none), Err(Refusal::NoTarget));
        assert_eq!(res(&req(Some("web"), Some("s1"), "/clear"), &rows, none), Err(Refusal::NoTarget));
        assert_eq!(res(&req(Some("  "), None, "/clear"), &rows, none), Err(Refusal::NoTarget));
    }

    #[test]
    fn a_project_or_an_anchored_session_names_a_local_row() {
        let rows = [row("web", Status::Done)];
        assert_eq!(res(&req(Some("web"), None, "/clear"), &rows, none), Ok((Command::Clear, "web".into())));
        assert_eq!(res(&req(None, Some("s1"), "exit"), &rows, |sid| (sid == "s1").then(|| "web".to_string())), Ok((Command::Exit, "web".into())));
        assert_eq!(res(&req(None, Some("s2"), "/clear"), &rows, none), Err(Refusal::NoSuchSession));
        assert_eq!(res(&req(Some("api"), None, "/clear"), &rows, none), Err(Refusal::NoSuchSession));
    }

    #[test]
    fn a_remote_or_working_row_is_refused() {
        let mut remote = row("AIR/web", Status::Done);
        remote.origin = Some("AIR".into());
        assert_eq!(res(&req(Some("AIR/web"), None, "/clear"), &[remote], none), Err(Refusal::NotLocal));
        assert_eq!(res(&req(Some("web"), None, "/clear"), &[row("web", Status::Working)], none), Err(Refusal::Busy));
        for status in [Status::Done, Status::Idle, Status::Blocked, Status::Waiting, Status::Error] {
            assert!(res(&req(Some("web"), None, "/clear"), &[row("web", status)], none).is_ok(), "{status:?}");
        }
    }

    #[test]
    fn a_main_agent_working_under_a_subagent_prompt_is_busy() {
        let mut gated = row("web", Status::Blocked);
        let base = crate::state::BaseState { status: Status::Working, label: String::new(), waiting_backstop_armed: false, state_entered_at: 0 };
        gated.subagent_gate = Some(crate::state::SubagentGate { base, blocked_since: 0, pending: Vec::new() });
        assert_eq!(res(&req(Some("web"), None, "/clear"), &[gated], none), Err(Refusal::Busy));
    }

    #[test]
    fn the_recheck_refuses_a_row_that_started_a_turn_or_went() {
        assert_eq!(still_free(Some(Status::Working)), Err(Refusal::Busy));
        assert_eq!(still_free(None), Err(Refusal::NoSuchSession));
        assert_eq!(still_free(Some(Status::Done)), Ok(()));
    }

    #[test]
    fn the_session_is_the_one_the_label_join_gives_the_row() {
        let rows = [row("bga", Status::Done), row("bga assistant", Status::Done)];
        let tab = |key: &str, title: &str| CloseTarget { key: key.into(), title: Some(title.into()) };
        let targets = [tab("w/1", "🟢 bga"), tab("w/2", "🟢 bga assistant [60%]")];
        assert_eq!(target_key(&rows, &targets, "bga assistant"), Ok("w/2".into()));
        assert_eq!(target_key(&rows, &targets, "bga"), Ok("w/1".into()));
        assert_eq!(target_key(&rows, &[tab("w/1", "🟢 web")], "bga"), Err(Refusal::Join(labels::NO_SESSION)));
        let twice = [tab("w/1", "🟢 bga"), tab("w/3", "⚪ bga")];
        assert_eq!(target_key(&rows, &twice, "bga").unwrap_err().slug(), "shared_title");
    }
}
