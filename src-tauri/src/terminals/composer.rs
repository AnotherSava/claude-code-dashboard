//! Where Claude Code's composer is on a screen of text, and what it holds.
//!
//! Shared by every adapter that types a command into the agent — `/exit` for a
//! close, or what `POST /api/session/command` asks for: each reads the pane its
//! own way, with Claude Code's faint placeholder runs already left out, and
//! hands the rows here, so the rule for finding the composer, and the sequence
//! that types into it, exist once whichever terminal drew it.

use std::time::Duration;

/// The text in the last composer among `lines`, trimmed, or `None` when none is
/// on screen. `lines` are a pane's rows top to bottom, with faint runs removed.
///
/// The composer is the last row that starts with the prompt glyph directly
/// under a `─` rule, through the rows down to the next rule. The last, because
/// a reply can quote a box shaped like the composer earlier in the output.
///
/// A rule is a row that begins and ends with `─`, since a session started with
/// a name has Claude Code draw it inside the top rule (`──── agterm ──`).
pub fn composer_in(lines: &[String]) -> Option<String> {
    let is_rule = |l: &str| l.trim().starts_with('─') && l.trim().ends_with('─');
    let prompt = |l: &str| l.trim_start().strip_prefix(['>', '❯']).map(str::to_string);
    let top = (1..lines.len()).rev().find(|&i| is_rule(&lines[i - 1]) && prompt(&lines[i]).is_some())?;
    let bottom = (top + 1..lines.len()).find(|&i| is_rule(&lines[i]))?;
    let mut text = prompt(&lines[top]).unwrap_or_default();
    for line in &lines[top + 1..bottom] {
        text.push('\n');
        text.push_str(line);
    }
    Some(text.trim().to_string())
}

/// The pause between the keystrokes of every typed command, a close's `/exit`
/// included, long enough for Claude Code to take each as typing rather than as
/// one paste.
pub const KEY_PAUSE: Duration = Duration::from_millis(600);

/// How long a close waits for the agent to exit after `/exit`, and how often it
/// looks.
pub const EXIT_WAIT: Duration = Duration::from_secs(30);
pub const EXIT_POLL: Duration = Duration::from_millis(500);

/// The built-in commands this dashboard will type into a session. An
/// allowlist, because typing into a person's terminal is safe only for text
/// whose effect is known: anything else would be a prompt sent on their behalf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `/clear`, which ends the conversation and starts a fresh one in place.
    Clear,
    /// `/exit`, which ends the agent; what a close types.
    Exit,
}

impl Command {
    /// The command `text` names, `/clear` or `/exit`, with or without the
    /// slash, or `None` for anything off the allowlist.
    pub fn parse(text: &str) -> Option<Command> {
        match text.trim().strip_prefix('/').unwrap_or(text.trim()) {
            "clear" => Some(Command::Clear),
            "exit" => Some(Command::Exit),
            _ => None,
        }
    }

    /// What is typed into the composer.
    pub fn text(self) -> &'static str {
        match self {
            Command::Clear => "/clear",
            Command::Exit => "/exit",
        }
    }
}

/// What typing a command sends into the session, which each terminal spells
/// its own way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    /// The command's text, typed rather than pasted.
    Command(Command),
    /// The Enter key.
    Enter,
}

/// Type `command` into an empty composer and press Enter once the composer
/// reads exactly its text, through the terminal's own `read` of the composer
/// and `send` of keys, waiting `pause` between the two.
///
/// Every refusal here keeps keystrokes off a person: a draft may be someone
/// typing (from this machine or, through tmux, from another), no composer means
/// something other than an agent waiting for input is on screen (a question
/// dialog replaces it), and a composer that does not read back the command
/// means something else took the typing, so Enter would submit it. What
/// follows (a close waiting for the agent to exit) is the caller's, since each
/// terminal sees that differently.
pub fn type_command(command: Command, mut read: impl FnMut() -> Result<Option<String>, String>, mut send: impl FnMut(Keys) -> Result<(), String>, pause: Duration) -> Result<(), String> {
    match read()? {
        Some(draft) if draft.is_empty() => {}
        Some(_) => return Err("the composer holds a draft, so someone may be typing into the session".to_string()),
        None => return Err("no Claude Code composer is on screen in the session's pane".to_string()),
    }
    send(Keys::Command(command))?;
    std::thread::sleep(pause);
    if read()?.as_deref() != Some(command.text()) {
        return Err(format!("the composer changed while {} was being typed, so Enter was not pressed", command.text()));
    }
    send(Keys::Enter)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run [`type_command`] for `command` against composer readings answered
    /// in turn, and return its result with the keys it sent.
    fn run_command(command: Command, readings: &[Option<&str>]) -> (Result<(), String>, Vec<Keys>) {
        let mut readings = readings.iter().map(|r| r.map(str::to_string));
        let mut sent = Vec::new();
        let result = type_command(command, || Ok(readings.next().expect("read more often than expected")), |k| {
            sent.push(k);
            Ok(())
        }, Duration::ZERO);
        (result, sent)
    }

    fn run(readings: &[Option<&str>]) -> (Result<(), String>, Vec<Keys>) {
        run_command(Command::Exit, readings)
    }

    #[test]
    fn an_empty_composer_gets_exit_then_enter() {
        assert_eq!(run(&[Some(""), Some("/exit")]), (Ok(()), vec![Keys::Command(Command::Exit), Keys::Enter]));
    }

    #[test]
    fn clear_is_typed_and_must_read_back_as_clear() {
        assert_eq!(run_command(Command::Clear, &[Some(""), Some("/clear")]), (Ok(()), vec![Keys::Command(Command::Clear), Keys::Enter]));
        let (result, sent) = run_command(Command::Clear, &[Some(""), Some("/exit")]);
        assert!(result.unwrap_err().contains("/clear was being typed"));
        assert_eq!(sent, [Keys::Command(Command::Clear)]);
    }

    #[test]
    fn only_the_allowlisted_commands_parse() {
        assert_eq!(Command::parse("/clear"), Some(Command::Clear));
        assert_eq!(Command::parse(" clear "), Some(Command::Clear));
        assert_eq!(Command::parse("/exit"), Some(Command::Exit));
        for other in ["/compact", "/clear now", "/clear\n/exit", "//clear", "", "/", "fix the build", "/CLEAR"] {
            assert_eq!(Command::parse(other), None, "{other:?}");
        }
        assert_eq!(Command::Clear.text(), "/clear");
    }

    #[test]
    fn a_session_name_drawn_in_the_top_rule_still_bounds_the_composer() {
        let lines: Vec<String> = ["✻ Brewed for 14s", "──── tripit ──", "❯ ", "──────────", "  ⏵⏵ auto mode on"].iter().map(|l| l.to_string()).collect();
        assert_eq!(composer_in(&lines), Some(String::new()));
        let drafted: Vec<String> = ["──── tripit ──", "❯ fix it", "──────────"].iter().map(|l| l.to_string()).collect();
        assert_eq!(composer_in(&drafted), Some("fix it".to_string()));
    }

    #[test]
    fn a_draft_or_no_composer_gets_no_keys() {
        let (result, sent) = run(&[Some("fix the build")]);
        assert!(result.unwrap_err().contains("draft"));
        assert!(sent.is_empty());
        let (result, sent) = run(&[None]);
        assert!(result.unwrap_err().contains("no Claude Code composer"));
        assert!(sent.is_empty());
    }

    #[test]
    fn enter_waits_for_an_exact_read_back() {
        for after in [Some("/exitx"), Some(""), None] {
            let (result, sent) = run(&[Some(""), after]);
            assert!(result.unwrap_err().contains("Enter was not pressed"), "{after:?}");
            assert_eq!(sent, [Keys::Command(Command::Exit)], "{after:?}");
        }
    }

    #[test]
    fn a_failed_read_or_send_stops_the_close() {
        let mut sent = Vec::new();
        assert_eq!(type_command(Command::Exit, || Err("pipe gone".to_string()), |k| {
            sent.push(k);
            Ok(())
        }, Duration::ZERO), Err("pipe gone".to_string()));
        assert!(sent.is_empty());
        assert_eq!(type_command(Command::Exit, || Ok(Some(String::new())), |_| Err("refused".to_string()), Duration::ZERO), Err("refused".to_string()));
    }
}
