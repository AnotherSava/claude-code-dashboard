//! agwinterm's control protocol, as pure functions.
//!
//! The pipe carries newline-delimited UTF-8 JSON, one request and one reply per
//! line. A request is `{"cmd", "target", "window", "args"}`, with `target` and
//! `window` at the top level and everything else in `args`; a reply is
//! `{"ok":true,"result":…}` or `{"ok":false,"error":"…"}`. Sessions belong to a
//! window, and both `tree` and a write's target are resolved inside the window
//! named in the request, so listing everything is `window.list` followed by one
//! `tree` per open window, and every write names its window.
//!
//! Everything here is a pure function so the parsing runs on any platform's test
//! build; `agwinterm` owns the pipe.

use serde_json::{json, Value};

use super::{LabelBudget, LabelTarget};

/// Where agwinterm listens when nothing says otherwise: a release build's pipe.
pub const DEFAULT_PIPE: &str = r"\\.\pipe\agwinterm";

/// The pipe path, from `AGWINTERM_PIPE` when it is set.
///
/// agwinterm sets that variable to the bare pipe name, while its own agent skill
/// describes it as the full path, so both forms are accepted.
pub fn pipe_path(env: Option<&str>) -> String {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(full) if full.starts_with(r"\\.\pipe\") => full.to_string(),
        Some(name) => format!(r"\\.\pipe\{name}"),
        None => DEFAULT_PIPE.to_string(),
    }
}

/// `ping`, whose reply names the build: `agwinterm <version>`.
pub fn ping_req() -> String {
    json!({ "cmd": "ping" }).to_string()
}

/// `window.list`.
pub fn windows_req() -> String {
    json!({ "cmd": "window.list" }).to_string()
}

/// `tree` for one window. Without a window it would answer for the frontmost
/// one only.
pub fn tree_req(window: &str) -> String {
    json!({ "cmd": "tree", "window": window }).to_string()
}

/// `session context <text>`, addressed by window and session id. agwinterm
/// reads the text from `args.context`.
pub fn context_req(window: &str, target: &str, text: &str) -> String {
    json!({ "cmd": "session.context", "target": target, "window": window, "args": { "context": text } }).to_string()
}

/// `session context --clear`.
pub fn clear_context_req(window: &str, target: &str) -> String {
    json!({ "cmd": "session.context", "target": target, "window": window, "args": { "clear": true } }).to_string()
}

/// `session type`: `text` written to the pane's input as keystrokes, a `\r`
/// being Enter. agwinterm refuses any other control byte.
pub fn type_req(window: &str, target: &str, text: &str) -> String {
    json!({ "cmd": "session.type", "target": target, "window": window, "args": { "text": text } }).to_string()
}

/// `session close`: close the session's tab and everything in it.
pub fn close_req(window: &str, target: &str) -> String {
    json!({ "cmd": "session.close", "target": target, "window": window }).to_string()
}

/// `session text --styles` over the last `lines` rows of `target`.
pub fn styled_text_req(window: &str, target: &str, lines: u32) -> String {
    json!({ "cmd": "session.text", "target": target, "window": window, "args": { "styles": true, "lines": lines } }).to_string()
}

/// A reply's `result`, or its `error` text.
pub fn parse_reply(line: &str) -> Result<Value, String> {
    let reply: Value = serde_json::from_str(line).map_err(|e| format!("unparseable reply: {e}"))?;
    match reply.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        Some(false) => Err(reply.get("error").and_then(Value::as_str).unwrap_or("refused with no reason").to_string()),
        None => Err(format!("reply carries no ok field: {line}")),
    }
}

/// The ids of the open windows in a `window.list` result. A closed window
/// answers "window not found" to `tree`, so it is left out here.
pub fn parse_windows(result: &Value) -> Vec<String> {
    open_windows(result).filter_map(|w| w.get("id").and_then(Value::as_str).map(str::to_string)).collect()
}

/// The window nodes of a `window.list` result that say they are open. A window
/// with no `open` flag counts as closed, so [`parse_windows`] and
/// [`active_window`] agree about which windows exist.
fn open_windows(result: &Value) -> impl Iterator<Item = &Value> {
    result.get("windows").and_then(Value::as_array).into_iter().flatten().filter(|w| w.get("open").and_then(Value::as_bool) == Some(true))
}

/// One session from a `tree` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgSession {
    /// The session id, which a write addresses.
    pub target: String,
    /// The focused pane's program title, as [`SessionView::title`].
    pub title: Option<String>,
    /// `None` when unset.
    pub context: Option<String>,
}

/// Every session in a `tree` result, across its workspaces.
pub fn parse_tree(result: &Value) -> Vec<AgSession> {
    let workspaces = result.get("workspaces").and_then(Value::as_array).into_iter().flatten();
    let sessions = workspaces.flat_map(|w| w.get("sessions").and_then(Value::as_array).into_iter().flatten());
    sessions.filter_map(parse_session).collect()
}

fn parse_session(node: &Value) -> Option<AgSession> {
    let text = |key: &str| node.get(key).and_then(Value::as_str).map(str::to_string);
    Some(AgSession { target: text("id")?, title: text("title"), context: text("context") })
}

/// The window agwinterm says is frontmost, from a `window.list` result: the open
/// window marked `active`.
///
/// That flag is agwinterm's live `_frontmostId`, which a library window takes the
/// moment it is activated, so it is read here rather than from the `Frontmost` of
/// the window index file, which is saved through the same settling writer as the
/// window files and can lag or lose a save the same way.
pub fn active_window(result: &Value) -> Option<String> {
    open_windows(result).find(|w| w.get("active").and_then(Value::as_bool) == Some(true)).and_then(|w| w.get("id")).and_then(Value::as_str).map(str::to_string)
}

/// What a `tree` says is running in one session's panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// Every pane is a shell agwinterm recognizes with no child process: a
    /// prompt, and no agent behind it.
    Bare,
    /// At least one pane is not: a child process runs in it, or its root is not
    /// a shell agwinterm recognizes, such as `wsl.exe` or an agent started
    /// directly. The tree spells both the same way.
    Occupied,
    /// The tree does not say.
    Unknown,
}

/// One session as a `tree` shows it, for naming it and for judging whether input
/// to the window went to that session's agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    /// Whether the session is the window's live selection.
    pub active: bool,
    pub shell: Shell,
    /// Whether an overlay is open over the session or one of its panes. An
    /// overlay over the selected session is drawn over its output and takes its
    /// input. The scratch cover does the same and the tree does not report it.
    pub covered: bool,
    /// The focused pane's program title, or `None` when it has none. For a pane
    /// running the agent this is the console title this dashboard writes,
    /// forwarded through whatever sits between, WSL and tmux included.
    pub title: Option<String>,
}

/// Session `id` in a `tree` result, or `None` when the tree does not list it.
///
/// The shell is read from `foregroundShells`, which agwinterm fills per pane with
/// the name of a live root shell that has no children, and `null` otherwise. The
/// title is `title`, the focused pane's program title, which agwinterm reports
/// even while a custom name hides it and leaves out when the pane has none.
pub fn view_of(result: &Value, id: &str) -> Option<SessionView> {
    let node = session_node(result, id)?;
    let shell = match node.get("foregroundShells").and_then(Value::as_array).map(Vec::as_slice) {
        None | Some([]) => Shell::Unknown,
        Some(panes) if panes.iter().all(Value::is_string) => Shell::Bare,
        Some(_) => Shell::Occupied,
    };
    let covered = covered(node);
    let title = node.get("title").and_then(Value::as_str).map(str::to_string);
    Some(SessionView { active: node.get("active").and_then(Value::as_bool) == Some(true), shell, covered, title })
}

/// Whether an overlay is open over session `node` or one of its panes.
fn covered(node: &Value) -> bool {
    node.get("overlay").and_then(Value::as_bool) == Some(true) || node.get("paneOverlays").and_then(Value::as_array).is_some_and(|o| !o.is_empty())
}

/// Session `id`'s node in a `tree` result.
pub fn session_node<'a>(result: &'a Value, id: &str) -> Option<&'a Value> {
    let workspaces = result.get("workspaces").and_then(Value::as_array).into_iter().flatten();
    let mut sessions = workspaces.flat_map(|w| w.get("sessions").and_then(Value::as_array).into_iter().flatten());
    sessions.find(|s| s.get("id").and_then(Value::as_str) == Some(id))
}

/// The pane a close types `/exit` into: the session's focused pane, the one its
/// `title` is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosePane {
    /// The pane's id. agwinterm resolves a session id to the pane that carries
    /// it, which in a split need not be the focused one, so a split names the
    /// focused pane by its own id.
    pub id: String,
    /// The pane's place in `foregroundShells`, which is in pane order.
    pub index: usize,
}

/// The pane to type into, or why session `node` must not be closed.
///
/// A close types into the agent rather than ending a process, because on this
/// machine the agent often runs in a WSL tmux pane the tab only attaches to:
/// closing the tab detaches the client and leaves the agent running, while a
/// clean `/exit` ends the agent and lets tmux tear its session down. Typing is
/// what makes these refusals necessary. It must reach the agent's composer: an
/// overlay over the session takes its input, and a focused pane that is a bare
/// shell is a prompt that would run the text as a command. It must not reach a
/// person mid-sentence, so the selected session of agwinterm's frontmost window
/// is left alone. And the title it was chosen by must still say CLEAN, since
/// the agent may have moved on since the label read. A draft in the composer
/// is checked separately, from the pane's text ([`composer_text`]), since the
/// tree does not carry it.
///
/// The close then ends the whole session, so a split is closed only where its
/// other pane is a bare shell: agwinterm closes a session without confirming,
/// and whatever ran beside the agent would go with it.
pub fn close_pane(node: &Value, in_front: bool) -> Result<ClosePane, String> {
    if in_front && node.get("active").and_then(Value::as_bool) == Some(true) {
        return Err("the session is on screen in agwinterm's frontmost window".to_string());
    }
    if covered(node) {
        return Err("an overlay covers the session and would take the input".to_string());
    }
    let title = node.get("title").and_then(Value::as_str);
    let idle = title.and_then(crate::terminal_title::parse_title).is_some_and(|r| r.status == crate::state::Status::Idle && !r.remote);
    if !idle {
        return Err("the session's title no longer shows it CLEAN".to_string());
    }
    let index = node.get("focusedPane").and_then(Value::as_u64).unwrap_or(0) as usize;
    if agent_gone(node, index) {
        return Err("the focused pane is a bare shell, so the agent is no longer running there".to_string());
    }
    let panes = node.get("paneIds").and_then(Value::as_array).map_or(1, Vec::len);
    if (0..panes).any(|i| i != index && !agent_gone(node, i)) {
        return Err("another pane in the session is running something the close would end".to_string());
    }
    let id = match node.get("paneIds").and_then(Value::as_array) {
        Some(ids) => ids.get(index).and_then(Value::as_str).ok_or("the tree names no focused pane")?.to_string(),
        None => node.get("id").and_then(Value::as_str).ok_or("the session has no id")?.to_string(),
    };
    Ok(ClosePane { id, index })
}

/// Whether pane `index` of session `node` has fallen back to a shell with no
/// child process, which is what the pane shows once the agent has exited,
/// tmux's client with it where there was one.
pub fn agent_gone(node: &Value, index: usize) -> bool {
    node.get("foregroundShells").and_then(Value::as_array).and_then(|panes| panes.get(index)).is_some_and(Value::is_string)
}

/// A row with the edge and fill tmux draws beyond a window smaller than the
/// client cut off. The edge is cut only where fill followed it, since a row
/// with no fill is the window's own, and its last column is content.
fn without_tmux_fill(row: &str) -> &str {
    let unfilled = row.trim_end_matches('·');
    if unfilled.len() == row.len() {
        return row;
    }
    unfilled.strip_suffix(['│', '┘']).unwrap_or(unfilled)
}

/// What Claude Code's composer holds, from a `session text --styles` result,
/// or `None` when no composer is on screen.
///
/// A close types `/exit` only into an empty composer, and checks again that it
/// holds exactly `/exit` before pressing Enter. Text there is a person's
/// draft, and the pane is the one place it shows whichever machine it is typed
/// from: tmux mirrors a client on the Mac into the same pane this tab draws,
/// so reading the pane covers the Mac's `⇄` tab without asking tmux anything.
///
/// The composer is the last row that starts with the prompt glyph directly
/// under a `─` rule, through the rows down to the next rule. Faint runs are
/// left out, being the placeholder suggestions Claude Code draws in an empty
/// composer, not text anyone typed. tmux draws its window at the size of its
/// smallest client, edges it with `│` (`┘` at the corner) and fills the rest of
/// a larger client with `·`, so while the Mac is attached to a session too,
/// every row this tab shows of it, the rules included, ends in that edge and
/// fill; [`without_tmux_fill`] cuts both first.
pub fn composer_text(styled: &Value) -> Option<String> {
    let rows = styled.get("rows").and_then(Value::as_array)?;
    let lines: Vec<String> = rows
        .iter()
        .map(|r| r.get("runs").and_then(Value::as_array).into_iter().flatten().filter(|run| run.get("faint").and_then(Value::as_bool) != Some(true)).filter_map(|run| run.get("text").and_then(Value::as_str)).collect::<String>())
        .map(|l| without_tmux_fill(&l).to_string())
        .collect();
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

/// The label targets for one window's sessions. The key is
/// `"{window}/{session}"`, so a write carries both halves of the address and
/// never resolves a session by its name.
pub fn targets_from(window: &str, sessions: &[AgSession]) -> Vec<LabelTarget> {
    sessions.iter().map(|s| LabelTarget { key: format!("{window}/{}", s.target), title: s.title.clone(), context: s.context.clone(), budget: LabelBudget::Utf16(CONTEXT_MAX_UTF16) }).collect()
}

/// The window and session halves of a key [`targets_from`] minted.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('/')
}

/// The longest context agwinterm accepts, in UTF-16 code units: its
/// `SessionContexts.MaxLength`, a display budget rather than a storage limit, so
/// `labels` cuts a longer line to it rather than having it refused.
pub const CONTEXT_MAX_UTF16: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;

    /// `tree` captured read-only from agwinterm 0.20.13.1 on 2026-10-02. The live
    /// tree had no split session, no context, no title and no renamed session, so
    /// session 3's name, context and title and the whole of session 4 are
    /// synthesized in the shape `HandleTree` emits: a status glyph as the
    /// surrogate-pair escape agwinterm writes, and a custom name beside the title
    /// the agent's console carries.
    const TREE: &str = r#"{"workspaces":[{"id":"dfa60a39-3fe2-4c90-ade4-a7bd1883b780","name":"workspace 1","active":true,"collapsed":false,"sessions":[
        {"id":"409d0a9a-e59d-47d4-a7e1-6270f9918635","name":"session 1","active":true,"status":"idle","statusChangedAt":1790932574,"foregroundShells":[null],"capturedCommands":{"409d0a9a-e59d-47d4-a7e1-6270f9918635":"\u0022C:\\WINDOWS\\system32\\wsl.exe\u0022 -d Ubuntu -- /mnt/c/src/tools/start.sh"},"paneCwds":{"409d0a9a-e59d-47d4-a7e1-6270f9918635":"C:\\src\\external\\agwinterm"}},
        {"id":"69892e75-7d33-4f18-b3af-df79e40ce7d9","name":"session 2","active":false,"status":"idle","statusChangedAt":1790932574,"foregroundShells":[null],"paneCwds":{"69892e75-7d33-4f18-b3af-df79e40ce7d9":"C:\\src\\claude"}},
        {"id":"c18abb52-0589-46ed-9010-4d2a552c7a86","name":"\ud83d\udd35 tauri-dashboard","active":false,"status":"idle","statusChangedAt":1790932574,"context":"Wire the context line \u2014 tests","title":"\ud83d\udd35 tauri-dashboard [68%]","paneCwds":{"c18abb52-0589-46ed-9010-4d2a552c7a86":"C:\\src\\tauri-dashboard"}},
        {"id":"7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f","name":"session 4","active":false,"status":"idle","statusChangedAt":1790932574,"paneCwds":{"7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f":"C:\\src\\transcripts"},"paneCount":2,"focusedPane":1,"splitRatios":[0.5],"paneIds":["7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f","b1f2e3d4-5c6b-4a79-8e8f-9a0b1c2d3e4f"],"axis":"vertical"}
    ]}]}"#;

    fn tree() -> Value {
        serde_json::from_str(TREE).unwrap()
    }

    #[test]
    fn pipe_path_takes_a_bare_name_or_a_full_path() {
        assert_eq!(pipe_path(Some("agwinterm-dev")), r"\\.\pipe\agwinterm-dev");
        assert_eq!(pipe_path(Some(r"\\.\pipe\agwinterm-dev")), r"\\.\pipe\agwinterm-dev");
    }

    #[test]
    fn pipe_path_falls_back_to_the_release_pipe() {
        assert_eq!(pipe_path(None), DEFAULT_PIPE);
        assert_eq!(pipe_path(Some("  ")), DEFAULT_PIPE);
    }

    #[test]
    fn parse_reply_ok_object_ok_string_and_error() {
        assert_eq!(parse_reply(r#"{"ok":true,"result":"agwinterm 0.20.13.1"}"#), Ok(json!("agwinterm 0.20.13.1")));
        assert_eq!(parse_reply(r#"{"ok":true,"result":{"session":"s","context":null}}"#), Ok(json!({ "session": "s", "context": null })));
        assert_eq!(parse_reply(r#"{"ok":false,"error":"session not found; nothing changed"}"#), Err("session not found; nothing changed".to_string()));
        assert!(parse_reply("not json").is_err());
        assert!(parse_reply(r#"{"result":1}"#).is_err());
    }

    #[test]
    fn parse_windows_keeps_only_open_ones() {
        let r = json!({ "windows": [{ "id": "499dc388", "name": "", "open": true, "active": true }, { "id": "closed", "open": false }] });
        assert_eq!(parse_windows(&r), ["499dc388"]);
    }

    fn shell_of(t: &Value, id: &str) -> Shell {
        view_of(t, id).map_or(Shell::Unknown, |v| v.shell)
    }

    #[test]
    fn a_session_is_a_bare_shell_only_when_every_pane_says_so() {
        let mut t = tree();
        assert_eq!(shell_of(&t, "409d0a9a-e59d-47d4-a7e1-6270f9918635"), Shell::Occupied, "the live tree: a pane with a child process");
        assert_eq!(shell_of(&t, "c18abb52-0589-46ed-9010-4d2a552c7a86"), Shell::Unknown, "no foregroundShells at all");
        assert_eq!(view_of(&t, "gone"), None, "a session not in the tree");
        t["workspaces"][0]["sessions"][1]["foregroundShells"] = json!(["pwsh"]);
        assert_eq!(shell_of(&t, "69892e75-7d33-4f18-b3af-df79e40ce7d9"), Shell::Bare);
        t["workspaces"][0]["sessions"][3]["foregroundShells"] = json!(["pwsh", null]);
        assert_eq!(shell_of(&t, "7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f"), Shell::Occupied, "a split with one busy pane");
    }

    #[test]
    fn a_sessions_title_is_its_focused_panes_program_title() {
        let t = tree();
        assert_eq!(view_of(&t, "c18abb52-0589-46ed-9010-4d2a552c7a86").unwrap().title.as_deref(), Some("🔵 tauri-dashboard [68%]"), "reported beside a name that hides it");
        assert_eq!(view_of(&t, "409d0a9a-e59d-47d4-a7e1-6270f9918635").unwrap().title, None, "absent when the pane has none");
    }

    #[test]
    fn the_tree_says_which_session_is_the_live_selection() {
        let t = tree();
        assert!(view_of(&t, "409d0a9a-e59d-47d4-a7e1-6270f9918635").unwrap().active, "the live tree's selected session");
        assert!(!view_of(&t, "69892e75-7d33-4f18-b3af-df79e40ce7d9").unwrap().active);
    }

    #[test]
    fn an_overlay_on_the_session_or_a_pane_covers_it() {
        let mut t = tree();
        let id = "409d0a9a-e59d-47d4-a7e1-6270f9918635";
        assert!(!view_of(&t, id).unwrap().covered, "the live tree has no overlay open");
        t["workspaces"][0]["sessions"][0]["overlay"] = json!(true);
        assert!(view_of(&t, id).unwrap().covered);
        let mut t = tree();
        t["workspaces"][0]["sessions"][0]["paneOverlays"] = json!(["left"]);
        assert!(view_of(&t, id).unwrap().covered);
    }

    #[test]
    fn the_frontmost_window_is_the_open_one_marked_active() {
        let r = json!({ "windows": [{ "id": "w1", "open": true, "active": false }, { "id": "w2", "open": true, "active": true }] });
        assert_eq!(active_window(&r).as_deref(), Some("w2"));
        assert_eq!(active_window(&json!({ "windows": [{ "id": "w1", "open": true, "active": false }] })), None, "no window in front");
        assert_eq!(active_window(&json!({ "windows": [{ "id": "w1", "open": false, "active": true }] })), None, "a closed window is not in front");
    }

    #[test]
    fn parse_tree_reads_the_live_shape() {
        let s = parse_tree(&tree());
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], AgSession { target: "409d0a9a-e59d-47d4-a7e1-6270f9918635".into(), title: None, context: None });
        assert_eq!(s[2], AgSession { target: "c18abb52-0589-46ed-9010-4d2a552c7a86".into(), title: Some("🔵 tauri-dashboard [68%]".into()), context: Some("Wire the context line — tests".into()) }, "the surrogate-pair escape decodes to the glyph");
        assert_eq!(s[3].target, "7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f", "a split is addressed by its session id");
    }

    #[test]
    fn targets_address_by_window_and_session() {
        let t = targets_from("499dc388-2497-4072-b1d9-05a339f9a03b", &parse_tree(&tree()));
        assert_eq!(t[2], LabelTarget { key: "499dc388-2497-4072-b1d9-05a339f9a03b/c18abb52-0589-46ed-9010-4d2a552c7a86".into(), title: Some("🔵 tauri-dashboard [68%]".into()), context: Some("Wire the context line — tests".into()), budget: LabelBudget::Utf16(CONTEXT_MAX_UTF16) });
        assert_eq!(split_key(&t[3].key), Some(("499dc388-2497-4072-b1d9-05a339f9a03b", "7e0d3c11-2a5b-4c8e-9f10-3b2a1c4d5e6f")));
    }

    #[test]
    fn request_wire_shapes() {
        let v = |s: String| serde_json::from_str::<Value>(&s).unwrap();
        assert_eq!(v(ping_req()), json!({ "cmd": "ping" }));
        assert_eq!(v(windows_req()), json!({ "cmd": "window.list" }));
        assert_eq!(v(tree_req("w")), json!({ "cmd": "tree", "window": "w" }));
        // `HandleSessionContext` reads `args.context` (`SessionContexts.Key`) and
        // `args.clear`, never both.
        assert_eq!(v(context_req("w", "s", "Fix the build")), json!({ "cmd": "session.context", "target": "s", "window": "w", "args": { "context": "Fix the build" } }));
        assert_eq!(v(clear_context_req("w", "s")), json!({ "cmd": "session.context", "target": "s", "window": "w", "args": { "clear": true } }));
        // `HandleType` reads `args.text`; `session.close` takes only its target.
        assert_eq!(v(type_req("w", "p", "/exit")), json!({ "cmd": "session.type", "target": "p", "window": "w", "args": { "text": "/exit" } }));
        assert_eq!(v(close_req("w", "s")), json!({ "cmd": "session.close", "target": "s", "window": "w" }));
        // `HandleText` reads `args.styles` and `args.lines`.
        assert_eq!(v(styled_text_req("w", "p", 30)), json!({ "cmd": "session.text", "target": "p", "window": "w", "args": { "styles": true, "lines": 30 } }));
        assert!(!ping_req().contains('\n'), "the caller frames each request with one newline");
    }

    /// A session node as `tree` shows a CLEAN agent under WSL and tmux: the
    /// root shell has a child, so its `foregroundShells` entry is `null`.
    fn clean_agent() -> Value {
        json!({ "id": "S", "active": false, "title": "⚫ web", "foregroundShells": [null] })
    }

    #[test]
    fn a_clean_agent_in_a_single_pane_is_typed_into_by_its_session_id() {
        assert_eq!(close_pane(&clean_agent(), true), Ok(ClosePane { id: "S".into(), index: 0 }));
    }

    #[test]
    fn a_split_names_its_focused_pane_by_its_own_id() {
        let mut n = clean_agent();
        n["paneIds"] = json!(["S", "P2"]);
        n["focusedPane"] = json!(1);
        n["foregroundShells"] = json!(["pwsh", null]);
        assert_eq!(close_pane(&n, false), Ok(ClosePane { id: "P2".into(), index: 1 }));
    }

    #[test]
    fn a_close_is_refused_wherever_the_keystrokes_could_land_somewhere_else() {
        let refused = |edit: &dyn Fn(&mut Value), in_front: bool| {
            let mut n = clean_agent();
            edit(&mut n);
            close_pane(&n, in_front).is_err()
        };
        assert!(refused(&|n| n["active"] = json!(true), true), "on screen in the frontmost window");
        assert!(!refused(&|n| n["active"] = json!(true), false), "selected in a window behind another is not on screen");
        assert!(refused(&|n| n["overlay"] = json!(true), false));
        assert!(refused(&|n| n["paneOverlays"] = json!(["left"]), false));
        assert!(refused(&|n| n["foregroundShells"] = json!(["pwsh"]), false), "a bare shell would run /exit as a command");
        assert!(refused(&|n| n["title"] = json!("🔵 web"), false), "the title moved off CLEAN");
        assert!(refused(&|n| n["title"] = json!("⇄ ⚫ web"), false), "a remote session is its own machine's to close");
        assert!(refused(&|n| { n.as_object_mut().unwrap().remove("title"); }, false));
        let split = |n: &mut Value, other: Value| {
            n["paneIds"] = json!(["S", "P2"]);
            n["focusedPane"] = json!(1);
            n["foregroundShells"] = json!([other, null]);
        };
        assert!(refused(&|n| split(n, Value::Null), false), "the other pane runs something the close would end");
        assert!(!refused(&|n| split(n, json!("pwsh")), false), "a bare shell beside the agent may go");
    }

    /// A `session text --styles` result from plain row texts, each one run;
    /// a row given as `(text, true)` is faint.
    fn styled(rows: &[(&str, bool)]) -> Value {
        let rows: Vec<Value> = rows.iter().enumerate().map(|(i, (t, faint))| json!({ "row": i, "runs": [{ "col": 0, "text": t, "faint": faint }] })).collect();
        json!({ "cols": 80, "rows": rows })
    }

    const RULE: &str = "────────────────";

    #[test]
    fn an_empty_composer_reads_empty() {
        // The shape read off a live idle session on 2026-10-15: past output, the
        // composer between two rules, then the status lines under it.
        let s = styled(&[("> an earlier prompt", false), ("reply", false), (RULE, false), (">", false), (RULE, false), ("  ⏵⏵ auto mode on", false)]);
        assert_eq!(composer_text(&s).as_deref(), Some(""));
    }

    #[test]
    fn the_last_composer_on_screen_is_the_one_read() {
        // A box shaped like the composer earlier in the output, such as one a
        // reply quoted, is not the live composer under it.
        let s = styled(&[(RULE, false), ("> quoted draft", false), (RULE, false), ("reply", false), (RULE, false), (">", false), (RULE, false)]);
        assert_eq!(composer_text(&s).as_deref(), Some(""));
    }

    #[test]
    fn a_draft_reads_back_whole_and_a_placeholder_does_not_count() {
        let s = styled(&[(RULE, false), ("> fix the", false), ("  build", false), (RULE, false)]);
        assert_eq!(composer_text(&s).as_deref(), Some("fix the\n  build"));
        let placeholder = json!({ "rows": [{ "runs": [{ "text": RULE }] }, { "runs": [{ "text": "> " }, { "text": "Try \"refactor\"", "faint": true }] }, { "runs": [{ "text": RULE }] }] });
        assert_eq!(composer_text(&placeholder).as_deref(), Some(""));
        assert_eq!(composer_text(&styled(&[(RULE, false), ("❯ /exit", false), (RULE, false)])).as_deref(), Some("/exit"));
    }

    #[test]
    fn tmux_fill_beyond_a_smaller_window_is_not_text() {
        // The shape read off a live session on 2026-10-16 that the Mac was also
        // attached to: tmux drew the window at the Mac's width, edged it, and
        // filled the rest of each row, and the rows under the window, with `·`.
        let fill = "·".repeat(20);
        let rule = format!("{RULE}│{fill}");
        let s = styled(&[(&rule, false), (&format!(">\u{a0}   │{fill}"), false), (&rule, false), (&format!("  Memos (43)  │{fill}"), false), (&format!("{RULE}┘{fill}"), false), (&fill, false)]);
        assert_eq!(composer_text(&s).as_deref(), Some(""));
        let s = styled(&[(&rule, false), (&format!("> /exit   │{fill}"), false), (&rule, false)]);
        assert_eq!(composer_text(&s).as_deref(), Some("/exit"));
        let s = styled(&[(RULE, false), ("> a draft ending │", false), (RULE, false)]);
        assert_eq!(composer_text(&s).as_deref(), Some("a draft ending │"), "an edge with no fill after it is the draft's own");
    }

    #[test]
    fn no_composer_on_screen_reads_none() {
        assert_eq!(composer_text(&styled(&[("PS C:\\> ", false)])), None);
        assert_eq!(composer_text(&styled(&[("> an earlier prompt", false), ("reply", false)])), None, "a past prompt has no rule above it");
        assert_eq!(composer_text(&styled(&[(RULE, false), ("> text", false)])), None, "no rule closes it");
    }

    #[test]
    fn the_agent_is_gone_once_its_pane_is_a_bare_shell() {
        let mut n = clean_agent();
        assert!(!agent_gone(&n, 0));
        n["foregroundShells"] = json!(["pwsh"]);
        assert!(agent_gone(&n, 0));
        assert!(!agent_gone(&n, 1), "a pane the tree does not list is not known to be a shell");
    }
}
