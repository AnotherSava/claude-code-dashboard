//! What the WSL tmux server answers, as pure functions, so the reading is
//! tested on every platform.
//!
//! The sessions here are the claude repo's remote sessions: a native
//! `claude.exe` inside a tmux pane its holder keeps alive, watched by a
//! terminal on this machine, by agterm on the Mac over ssh, or by both. The
//! dashboard's console title reaches tmux as the pane's title, which is what
//! names the session's row.

use super::CloseTarget;

/// The shell script that lists every pane, then every client with the machine
/// it is sitting at. One `P` line per pane (session, its pane title) and one
/// `C` line per client (session, origin), tab-separated. A server that is not
/// running lists nothing and exits 0, so silence is an answer.
///
/// The origin is the `REMOTE_SESSION_ORIGIN` the claude repo's attach scripts
/// export into each client's environment (`remote_session_client_origins` in
/// its `claude/remote-session/lib.sh`); tmux cannot report it, so it is read
/// off the client's own process. A client that recorded none reads `unknown`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const LIST_SCRIPT: &str = "tmux has-session 2>/dev/null || exit 0
tmux list-panes -a -F 'P\t#{session_name}\t#{pane_title}' || exit 1
tmux list-clients -F '#{client_pid} #{session_name}' | while read -r pid s; do o=$(tr '\\0' '\\n' < \"/proc/$pid/environ\" 2>/dev/null | sed -n 's/^REMOTE_SESSION_ORIGIN=//p'); printf 'C\\t%s\\t%s\\n' \"$s\" \"${o:-unknown}\"; done";

/// The origin the claude repo's scripts record for a client on this machine.
const ORIGIN_HERE: &str = "windows";

/// One tmux session as [`LIST_SCRIPT`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxSession {
    pub name: String,
    /// Every pane's title, in tmux's order.
    pub pane_titles: Vec<String>,
    /// The machine each attached client is at, `unknown` where it recorded none.
    pub origins: Vec<String>,
}

/// The sessions in [`LIST_SCRIPT`]'s output, in the order their first pane
/// was listed.
pub fn parse_list(out: &str) -> Vec<TmuxSession> {
    let mut sessions: Vec<TmuxSession> = Vec::new();
    for line in out.lines() {
        let mut fields = line.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("P"), Some(name), title) => entry(&mut sessions, name).pane_titles.push(title.unwrap_or("").to_string()),
            (Some("C"), Some(name), Some(origin)) => entry(&mut sessions, name).origins.push(origin.to_string()),
            _ => {}
        }
    }
    sessions
}

/// The session named `name` in `sessions`, added at the end if absent.
fn entry<'a>(sessions: &'a mut Vec<TmuxSession>, name: &str) -> &'a mut TmuxSession {
    let i = match sessions.iter().position(|s| s.name == name) {
        Some(i) => i,
        None => {
            sessions.push(TmuxSession { name: name.to_string(), pane_titles: Vec::new(), origins: Vec::new() });
            sessions.len() - 1
        }
    };
    &mut sessions[i]
}

/// Whether a terminal on this machine may be showing the session: a client
/// that says so, or one that recorded nothing, which could be.
fn shown_here(session: &TmuxSession) -> bool {
    session.origins.iter().any(|o| o == ORIGIN_HERE || o == "unknown")
}

/// The sessions only another machine's tabs, or none, are showing, keyed by
/// session name.
///
/// A session a terminal here shows is that terminal's to close, and is left
/// out: agwinterm lists it with the same title, and two sessions whose titles
/// name one row make the join refuse the row (`shared_title`), which would
/// stop agwinterm's own close of it.
pub fn close_targets(sessions: &[TmuxSession]) -> Vec<CloseTarget> {
    sessions.iter().filter(|s| !shown_here(s)).map(|s| CloseTarget { key: s.name.clone(), title: s.pane_titles.first().cloned() }).collect()
}

/// Why `session` must not be closed by typing into it, or `None` when it may.
/// Read again just before the close, since a terminal here may have attached,
/// or a prompt from the Mac started a turn, since the targets were listed.
pub fn close_refusal(session: &TmuxSession) -> Option<String> {
    if shown_here(session) {
        return Some("a terminal on this machine is attached to the session now, so that terminal closes it".to_string());
    }
    match session.pane_titles.as_slice() {
        [title] if crate::terminal_title::shows_clean_here(title) => None,
        [_] => Some("the session's title no longer shows it CLEAN".to_string()),
        panes => Some(format!("the tmux session holds {} panes, and /exit would reach only one of them", panes.len())),
    }
}

/// Whether `tmux has-session`'s stderr says the session is gone, which with
/// the server gone too is still gone. Anything else is a failure to ask.
pub fn says_gone(stderr: &str) -> bool {
    stderr.contains("can't find session") || stderr.contains("no server running")
}

/// A `capture-pane -e -p` capture as plain rows, with every faint run left out
/// and every escape sequence removed.
///
/// Faint is how Claude Code draws the placeholder suggestion in an empty
/// composer, so a placeholder must not read as a draft. tmux writes SGR 2 for
/// it and turns it off with SGR 0 or 22. The colour forms are skipped whole
/// (`38;5;N`, `38;2;R;G;B`, and the same for 48 and 58), since a `2` inside
/// one is a colour component and not faint.
pub fn plain_lines(capture: &str) -> Vec<String> {
    let mut faint = false;
    capture
        .lines()
        .map(|line| {
            let mut out = String::new();
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '\u{1b}' {
                    if !faint {
                        out.push(c);
                    }
                    continue;
                }
                if chars.peek() != Some(&'[') {
                    chars.next();
                    continue;
                }
                chars.next();
                let mut params = String::new();
                let mut last = None;
                for p in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&p) {
                        last = Some(p);
                        break;
                    }
                    params.push(p);
                }
                if last == Some('m') {
                    faint = apply_sgr(&params, faint);
                }
            }
            out
        })
        .collect()
}

/// The faint state after one SGR sequence's parameters.
fn apply_sgr(params: &str, mut faint: bool) -> bool {
    let codes: Vec<&str> = params.split(';').collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            "" | "0" | "22" => faint = false,
            "2" => faint = true,
            "38" | "48" | "58" => {
                i += match codes.get(i + 1) {
                    Some(&"5") => 2,
                    Some(&"2") => 4,
                    _ => 0,
                };
            }
            _ => {}
        }
        i += 1;
    }
    faint
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::composer::composer_in;

    /// [`LIST_SCRIPT`]'s output, captured 2026-10-19 from the holder's server
    /// and trimmed to four sessions: the keepalive, one watched from here, one
    /// watched only from the Mac, and one watched from both.
    const LIST: &str = "P\t_holder\tCHROME\nP\tcc-claude\t⚫ claude\nP\tcc-landlord\t✋ landlord\nP\tcc-trips\t🔵 trips\nC\tcc-landlord\twindows\nC\tcc-trips\twindows\nC\tcc-claude\tmac\nC\tcc-trips\tmac\n";

    fn session(name: &str, titles: &[&str], origins: &[&str]) -> TmuxSession {
        TmuxSession { name: name.into(), pane_titles: titles.iter().map(|t| t.to_string()).collect(), origins: origins.iter().map(|o| o.to_string()).collect() }
    }

    #[test]
    fn the_list_pairs_each_session_with_its_panes_and_clients() {
        let s = parse_list(LIST);
        assert_eq!(s.len(), 4);
        assert_eq!(s[1], session("cc-claude", &["⚫ claude"], &["mac"]));
        assert_eq!(s[3], session("cc-trips", &["🔵 trips"], &["windows", "mac"]));
        assert_eq!(s[0], session("_holder", &["CHROME"], &[]));
    }

    #[test]
    fn only_sessions_no_terminal_here_shows_are_offered() {
        let keys: Vec<String> = close_targets(&parse_list(LIST)).into_iter().map(|t| t.key).collect();
        assert_eq!(keys, ["_holder", "cc-claude"], "cc-landlord and cc-trips each have a client on this machine");
    }

    #[test]
    fn a_client_that_recorded_no_machine_could_be_here() {
        let s = session("cc-x", &["⚫ x"], &["mac", "unknown"]);
        assert!(close_targets(std::slice::from_ref(&s)).is_empty());
        assert!(close_refusal(&s).is_some());
    }

    #[test]
    fn an_unwatched_session_is_offered_with_its_title() {
        assert_eq!(close_targets(&[session("cc-x", &["⚫ x"], &[])]), [CloseTarget { key: "cc-x".into(), title: Some("⚫ x".into()) }]);
    }

    #[test]
    fn a_session_whose_title_left_clean_is_refused() {
        assert!(close_refusal(&session("cc-x", &["🔵 x"], &["mac"])).unwrap().contains("no longer shows it CLEAN"));
        assert!(close_refusal(&session("cc-x", &["⇄ ⚫ x"], &["mac"])).is_some(), "a title another machine wrote is not ours to read as CLEAN");
    }

    #[test]
    fn a_split_session_is_refused() {
        assert_eq!(close_refusal(&session("cc-x", &["⚫ x"], &["mac"])), None);
        assert!(close_refusal(&session("cc-x", &["⚫ x", "bash"], &["mac"])).unwrap().contains("2 panes"));
    }

    #[test]
    fn a_missing_session_or_server_is_gone() {
        assert!(says_gone("can't find session: cc-x\n"));
        assert!(says_gone("no server running on /tmp/tmux-1000/default\n"));
        assert!(!says_gone("wsl: The distribution was not found\n"));
    }

    #[test]
    fn faint_text_is_dropped_and_colour_components_are_not_faint() {
        // The shape tmux 3.4's capture-pane -e gives for
        // `\e[38;2;1;3;2mA\e[2mfaint\e[22mB\e[2;38;5;2mC\e[0mD`, measured with
        // a blue of 3; a blue of 2 is what makes a colour skip one short read
        // as faint.
        let capture = "\u{1b}[38;2;1;3;2mA\u{1b}[2mfaint\u{1b}[0m\u{1b}[38;2;1;3;2mB\u{1b}[2m\u{1b}[38;5;2mC\u{1b}[0mD";
        assert_eq!(plain_lines(capture), ["ABD"]);
        assert_eq!(plain_lines("\u{1b}[2mA\u{1b}[22mB"), ["B"], "SGR 22 also ends faint");
    }

    /// The bottom of `cc-claude`'s pane, captured with `capture-pane -e -p` on
    /// 2026-10-19: an empty composer with the cursor in it, between two rules.
    const RULE: &str = "\u{1b}[38;2;136;136;136m────────────────────";
    const EMPTY: &str = "\u{1b}[39m>\u{a0}\u{1b}[7m \u{1b}[0m";

    #[test]
    fn an_empty_composer_reads_empty() {
        let capture = [RULE, EMPTY, RULE, "\u{1b}[39m  \u{1b}[38;2;153;153;153mMemos (44)\u{1b}[39m"].join("\n");
        assert_eq!(composer_in(&plain_lines(&capture)).as_deref(), Some(""));
    }

    #[test]
    fn a_faint_placeholder_is_not_a_draft_and_typed_text_is() {
        let placeholder = [RULE, "\u{1b}[39m>\u{a0}\u{1b}[2mTry \"fix the build\"\u{1b}[0m", RULE].join("\n");
        assert_eq!(composer_in(&plain_lines(&placeholder)).as_deref(), Some(""));
        let typed = [RULE, "\u{1b}[39m>\u{a0}/exit\u{1b}[7m \u{1b}[0m", RULE].join("\n");
        assert_eq!(composer_in(&plain_lines(&typed)).as_deref(), Some("/exit"));
    }
}
