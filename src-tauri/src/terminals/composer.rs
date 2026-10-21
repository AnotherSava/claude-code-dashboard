//! Where Claude Code's composer is on a screen of text, and what it holds.
//!
//! Shared by every adapter whose close types `/exit` into the agent: each reads
//! the pane its own way, with Claude Code's faint placeholder runs already left
//! out, and hands the rows here, so the rule for finding the composer, and the
//! sequence that types into it, exist once whichever terminal drew it.

use std::time::Duration;

/// The text in the last composer among `lines`, trimmed, or `None` when none is
/// on screen. `lines` are a pane's rows top to bottom, with faint runs removed.
///
/// The composer is the last row that starts with the prompt glyph directly
/// under a `─` rule, through the rows down to the next rule. The last, because
/// a reply can quote a box shaped like the composer earlier in the output.
pub fn composer_in(lines: &[String]) -> Option<String> {
    let is_rule = |l: &str| !l.trim().is_empty() && l.trim().chars().all(|c| c == '─');
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

/// The pause between the keystrokes of a close, long enough for Claude Code to
/// take each as typing rather than as one paste.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const KEY_PAUSE: Duration = Duration::from_millis(600);

/// How long a close waits for the agent to exit after `/exit`, and how often it
/// looks.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const EXIT_WAIT: Duration = Duration::from_secs(30);
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const EXIT_POLL: Duration = Duration::from_millis(500);

/// What a close sends into the session, which each terminal spells its own way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    /// The text `/exit`, typed rather than pasted.
    Exit,
    /// The Enter key.
    Enter,
}

/// Type `/exit` into an empty composer and press Enter once the composer reads
/// exactly that, through the terminal's own `read` of the composer and `send`
/// of keys, waiting `pause` between the two.
///
/// Every refusal here keeps keystrokes off a person: a draft may be someone
/// typing (from this machine or, through tmux, from another), no composer means
/// something other than an agent waiting for input is on screen, and a
/// composer that does not read back `/exit` means something else took the
/// typing, so Enter would submit it. Waiting for the agent to exit is the
/// caller's, since each terminal sees that differently.
pub fn type_exit(mut read: impl FnMut() -> Result<Option<String>, String>, mut send: impl FnMut(Keys) -> Result<(), String>, pause: Duration) -> Result<(), String> {
    match read()? {
        Some(draft) if draft.is_empty() => {}
        Some(_) => return Err("the composer holds a draft, so someone may be typing into the session".to_string()),
        None => return Err("no Claude Code composer is on screen in the session's pane".to_string()),
    }
    send(Keys::Exit)?;
    std::thread::sleep(pause);
    if read()?.as_deref() != Some("/exit") {
        return Err("the composer changed while /exit was being typed, so Enter was not pressed".to_string());
    }
    send(Keys::Enter)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run [`type_exit`] against composer readings answered in turn, and
    /// return its result with the keys it sent.
    fn run(readings: &[Option<&str>]) -> (Result<(), String>, Vec<Keys>) {
        let mut readings = readings.iter().map(|r| r.map(str::to_string));
        let mut sent = Vec::new();
        let result = type_exit(|| Ok(readings.next().expect("read more often than expected")), |k| {
            sent.push(k);
            Ok(())
        }, Duration::ZERO);
        (result, sent)
    }

    #[test]
    fn an_empty_composer_gets_exit_then_enter() {
        assert_eq!(run(&[Some(""), Some("/exit")]), (Ok(()), vec![Keys::Exit, Keys::Enter]));
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
            assert_eq!(sent, [Keys::Exit], "{after:?}");
        }
    }

    #[test]
    fn a_failed_read_or_send_stops_the_close() {
        let mut sent = Vec::new();
        assert_eq!(type_exit(|| Err("pipe gone".to_string()), |k| {
            sent.push(k);
            Ok(())
        }, Duration::ZERO), Err("pipe gone".to_string()));
        assert!(sent.is_empty());
        assert_eq!(type_exit(|| Ok(Some(String::new())), |_| Err("refused".to_string()), Duration::ZERO), Err("refused".to_string()));
    }
}
