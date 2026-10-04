//! What agterm can tell the person verdict about a switch or an input.
//!
//! Two kinds of fact, from two places. What agterm itself knows comes from its
//! control answers, read here as pure functions so they run in every platform's
//! test build: which window is its active one (`window list`), and what runs in
//! a session and whether anything is drawn over it (`tree`). Which application
//! is in front, and since when, is the system's, and comes from `NSWorkspace`.
//!
//! **Since when agterm has been in front is recorded from notifications, not
//! sampled.** `NSWorkspaceDidActivateApplicationNotification` arrives at every
//! activation, so the record changes exactly when the front application does:
//! nothing wakes while nothing changes, and an away-and-back faster than any
//! sampling interval is still seen, which matters because the rule it feeds
//! exists for the one click that both brings agterm forward and switches its
//! session. It is the macOS counterpart of the `EVENT_SYSTEM_FOREGROUND` hook
//! both Windows adapters keep.
//!
//! **The record is agterm's application, and it is reported for whichever window
//! agterm names active.** The verdict asks since when the *window* a switch
//! happened in held the foreground, and macOS reports no window activation to
//! another process. With one window open the window comes forward exactly when
//! agterm does. With several, the window agterm's `window list` marks active is
//! credited while agterm is the frontmost application, with agterm's own
//! activation as its start, and that start can be earlier than the truth: a click
//! that brings a background agterm window forward while another agterm window was
//! in front raises no notification, so the activation rule does not catch the
//! click that both raised that window and switched its session, and the session
//! it left can be marked read. Accepted, since such a row only reads as read
//! early and every later turn raises it again; a record sampled from `window
//! list` would not close it either, since a window can go behind and come back
//! between two samples and keep its old start. The input side has the same
//! limit: the input rules judge against agterm's activation too, so they do not
//! refuse the click that raised the background window, and that click, and
//! input up to [`super::SWITCH_RELEASE_MS`] after it, is credited to the session
//! the raised window has selected — an arrival, marked read at most that much
//! early, on a session the user is looking at from then on. Whether `window
//! list`'s `active` follows a window brought forward by the mouse, and not only
//! by `window select`, is unmeasured on agterm (`learnings/agterm.md` measured
//! the latter only); it is assumed by parity with agwinterm, whose `window.list`
//! marks its frontmost window active. Were it to lag, input typed into the new
//! window would be credited to the old one's selection.
//!
//! **Since when agterm came forward is recorded late, and that refuses more.** The
//! instant is taken when the notification reaches this process's main thread,
//! after the click that made the activation. Input is judged against the stretch
//! agterm holds the foreground in now, never matched to an earlier one, so the
//! activating click, which lands before the late record, is refused as input
//! made before the window came forward.
//!
//! The `tree` fields, and where each is known from. `foreground` is the argv of
//! the process in front of the session's terminal, measured on agterm 2026-09-24
//! (`learnings/agterm.md`): a tab that exec'd into ssh reported ssh's argv.
//! `paneOverlays` lists a session's pane overlay terminals, which agterm's
//! control API reports in `tree` since its pane-scoped overlays, per agwinterm's
//! parity table.
//!
//! **A scratch overlay is reported through `surfaces`, not through `overlay`**,
//! measured on agterm 0.25.0 (commit 94ab03f2) 2026-10-03 by toggling one and
//! reading `tree` on either side. Opening it left `overlay` at `false` and added
//! a `surfaces` entry with `kind: "scratch"` and `visible: true`, while the
//! `left` surface went invisible; `scratch: true` appeared on the node too.
//! Closing it put `visible: false` on that same entry rather than removing it,
//! which is why [`cover`] reads the surface's `visible` and not the node's
//! `scratch` or the entry's presence.
//!
//! `overlay` is a real boolean on every session node and stayed `false`
//! throughout, so what sets it is unmeasured — 0.25.0 exposes no overlay
//! command, `session scratch` being the only one of that shape. It is read
//! anyway, as `paneOverlays` is, because that is how agwinterm, the Windows
//! port of agterm's control model, spells it in its own `tree`, and because a
//! node without either key reads as clear, as agwinterm's own reading does.

use serde_json::Value;

use super::{Cover, Front, Occupant, Since};

/// The shells a session can sit idle in. A login shell's `argv[0]` carries a
/// leading `-`, which is stripped before the comparison.
///
/// An argument that is not a flag disqualifies it: a shell running a script, or
/// a `-c` command, is a program in front of the terminal.
/// [`super::person_verdict`] refuses [`Occupant::Shell`] under every naming —
/// the rule that stops a prompt left in a tab inheriting its agent's title — so
/// reading such a shell as unoccupied silently refuses every observation from
/// that tab. agterm draws the same line in its own
/// `CommandRestore.isIdleShell`, and this reads agterm's `foreground`.
///
/// What it cannot say is that the shell sits at a prompt: a builtin runs in the
/// shell process and leaves `argv` untouched, so a shell mid-builtin is
/// indistinguishable from an idle one here. The verdict needs only the weaker
/// claim — that no *other* program is in front — so nothing downstream relies
/// on the stronger one.
const SHELLS: [&str; 7] = ["zsh", "bash", "sh", "fish", "nu", "tcsh", "ksh"];

/// The interpreters an npm install of Claude Code runs under, with the script
/// as their first argument.
const INTERPRETERS: [&str; 2] = ["node", "bun"];

/// What runs in a session, from its `tree` node: the agent when the program in
/// front of its terminal is Claude Code, run directly or as a script under
/// [`INTERPRETERS`], a shell when it is a shell carrying nothing but flags, and
/// another program otherwise — a shell running a script or a `-c` command
/// included, for the reason [`SHELLS`] gives. Unknown when the tree does not
/// list the session or does not say.
pub fn occupant(node: Option<&Value>) -> Occupant {
    let Some(node) = node else { return Occupant::Unknown("not_in_tree") };
    let Some(argv) = node.get("foreground").and_then(Value::as_array).filter(|argv| !argv.is_empty()) else {
        return Occupant::Unknown("no_foreground");
    };
    let arg = |i: usize| argv.get(i).and_then(Value::as_str).unwrap_or_default().trim();
    let name = |path: &str| path.rsplit('/').next().unwrap_or_default().trim_start_matches('-').to_string();
    let program = name(arg(0));
    let script = arg(1);
    if program.is_empty() {
        Occupant::Unknown("no_foreground")
    } else if crate::liveness::is_claude_image(&program) || (INTERPRETERS.contains(&program.as_str()) && (crate::liveness::is_claude_image(&name(script)) || script.contains("/@anthropic-ai/claude-code/"))) {
        Occupant::Agent
    } else if SHELLS.contains(&program.as_str()) && argv.iter().skip(1).all(|a| a.as_str().unwrap_or_default().trim().starts_with('-')) {
        Occupant::Shell
    } else {
        Occupant::Other
    }
}

/// Whether an overlay terminal is drawn over a session, from its `tree` node:
/// covered when a scratch surface is visible, when `overlay` is `true`, or when
/// `paneOverlays` lists one; clear otherwise, the key being absent included,
/// and unknown when the tree does not list the session. See the module doc for
/// which of those agterm was measured to report and why an absent key reads as
/// clear.
///
/// The scratch test is on the surface's `visible`, never on its presence: a
/// hidden scratch shell stays alive, so its entry outlives the overlay being on
/// screen and presence alone would read a session as covered for the rest of
/// its life.
pub fn cover(node: Option<&Value>) -> Cover {
    let Some(node) = node else { return Cover::Unknown };
    let pane_overlays = node.get("paneOverlays").and_then(Value::as_array).is_some_and(|o| !o.is_empty());
    let scratch_shown = node
        .get("surfaces")
        .and_then(Value::as_array)
        .is_some_and(|s| s.iter().any(|f| f.get("kind").and_then(Value::as_str) == Some("scratch") && f.get("visible").and_then(Value::as_bool) == Some(true)));
    if scratch_shown || node.get("overlay").and_then(Value::as_bool) == Some(true) || pane_overlays {
        Cover::Covered
    } else {
        Cover::Clear
    }
}

/// The id of agterm's active window in a `window list --json` answer, or `None`
/// when no open window is marked active.
pub fn active_window(list: &Value) -> Option<String> {
    crate::agterm::open_windows(list).find(|w| w.get("active").and_then(Value::as_bool) == Some(true)).and_then(|w| w.get("id")).and_then(Value::as_str).map(str::to_string)
}

/// Which application the system says is in front.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFront {
    Agterm,
    Other,
    /// The system named no application.
    Unreadable,
}

/// agterm's windows as a `window list --json` answer describes them, or
/// unanswered when there was no answer.
pub fn windows(list: Option<&Value>) -> Windows {
    match list {
        Some(list) => Windows::Listed { active: active_window(list) },
        None => Windows::Unanswered,
    }
}

/// agterm's windows, as its `window list` answered.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Windows {
    Listed {
        /// The open window agterm marks active, or `None` when it marks none.
        active: Option<String>,
    },
    /// agterm did not answer.
    Unanswered,
}

/// Whether `window` is in front: agterm is the frontmost application, and the
/// window is its active one.
pub fn front(app: AppFront, windows: &Windows, window: &str) -> Front {
    match (app, windows) {
        (AppFront::Unreadable, _) => Front::Unknown("frontmost_unreadable"),
        (AppFront::Other, _) => Front::No,
        (AppFront::Agterm, Windows::Unanswered) => Front::Unknown("window_list_unanswered"),
        (AppFront::Agterm, Windows::Listed { active: None, .. }) => Front::Unknown("no_active_window"),
        (AppFront::Agterm, Windows::Listed { active: Some(active), .. }) if active == window => Front::Yes,
        (AppFront::Agterm, Windows::Listed { .. }) => Front::No,
    }
}

/// Whether an application bundle at `path` is agterm's, by its bundle directory
/// name, which is what the cask installs and a local build produces alike.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn is_agterm_bundle(path: &str) -> bool {
    path.trim_end_matches('/').rsplit('/').next().is_some_and(|name| name.eq_ignore_ascii_case("agterm.app"))
}

/// Since when agterm has been the frontmost application, from the activations
/// seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activation {
    agterm_since: Option<i64>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Activation {
    pub const fn new() -> Self {
        Self { agterm_since: None }
    }

    /// An application was activated at `at_ms`: agterm, or another one.
    pub fn activated(&mut self, agterm: bool, at_ms: i64) {
        self.agterm_since = agterm.then_some(at_ms);
    }

    /// Since when agterm has been the frontmost application, unrecorded while
    /// another one is.
    pub fn since(&self) -> Since {
        self.agterm_since.map_or(Since::Unrecorded, Since::At)
    }
}

/// The activations seen, written from the notification block and read when an
/// observation's facts are gathered.
#[cfg(target_os = "macos")]
static ACTIVATION: std::sync::Mutex<Activation> = std::sync::Mutex::new(Activation::new());

#[cfg(target_os = "macos")]
fn activation() -> std::sync::MutexGuard<'static, Activation> {
    ACTIVATION.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Since when agterm has been the frontmost application.
#[cfg(target_os = "macos")]
pub fn front_since() -> Since {
    activation().since()
}

/// Where each of agterm's activations is reported, for the adapter's check of
/// which session its window has selected once it has been in front a while.
#[cfg(target_os = "macos")]
static ACTIVATIONS: std::sync::OnceLock<std::sync::mpsc::Sender<i64>> = std::sync::OnceLock::new();

/// Record an application's activation at `at_ms`, and report agterm's. The one
/// writer, shared by the notifications and the record of what was in front when
/// they started.
#[cfg(target_os = "macos")]
fn record_activation(agterm: bool, at_ms: i64) {
    activation().activated(agterm, at_ms);
    if let Some(tx) = ACTIVATIONS.get().filter(|_| agterm) {
        let _ = tx.send(at_ms);
    }
}

#[cfg(target_os = "macos")]
pub use appkit::{agterm_frontmost, observe_activation};

/// The `NSWorkspace` calls, through the Objective-C runtime's C entry points.
///
/// Declared by hand like the rest of this crate's platform calls. Every message
/// sent here takes the receiver, the selector and object arguments and answers
/// an object or a C string pointer, so one `objc_msgSend` signature per argument
/// count covers them, and a nil receiver answers nil rather than failing.
#[cfg(target_os = "macos")]
mod appkit {
    use std::ffi::{c_char, c_void, CStr};

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {
        static NSWorkspaceDidActivateApplicationNotification: Id;
        static NSWorkspaceApplicationKey: Id;
    }

    // The block runtime's class for a block with no captures, from libSystem.
    extern "C" {
        static _NSConcreteGlobalBlock: [usize; 32];
    }

    /// `[receiver selector]`.
    unsafe fn send(receiver: Id, selector: &CStr) -> Id {
        let call: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        call(receiver, sel_registerName(selector.as_ptr()))
    }

    /// `[receiver selector:a]`.
    unsafe fn send1(receiver: Id, selector: &CStr, a: Id) -> Id {
        let call: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        call(receiver, sel_registerName(selector.as_ptr()), a)
    }

    /// `[receiver selector:a b:b c:c d:d]`.
    unsafe fn send4(receiver: Id, selector: &CStr, a: Id, b: Id, c: Id, d: Id) -> Id {
        let call: unsafe extern "C" fn(Id, Sel, Id, Id, Id, Id) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        call(receiver, sel_registerName(selector.as_ptr()), a, b, c, d)
    }

    unsafe fn shared_workspace() -> Id {
        send(objc_getClass(c"NSWorkspace".as_ptr()), c"sharedWorkspace")
    }

    /// Whether `app`, an `NSRunningApplication`, is agterm, or `None` for no
    /// application at all.
    unsafe fn is_agterm(app: Id) -> Option<bool> {
        if app.is_null() {
            return None;
        }
        let path = send(send(app, c"bundleURL"), c"path");
        let utf8 = send(path, c"UTF8String") as *const c_char;
        if utf8.is_null() {
            return Some(false);
        }
        Some(super::is_agterm_bundle(&CStr::from_ptr(utf8).to_string_lossy()))
    }

    /// Which application is in front now.
    pub fn agterm_frontmost() -> super::AppFront {
        // SAFETY: messages to `NSWorkspace`'s shared instance and the objects it
        // answers, inside a pool of their own. AppKit's headers document the
        // workspace's running-application properties and `NSRunningApplication`
        // as callable from background threads, answered atomically and refreshed
        // as the main run loop turns, which this app's always does. Only the
        // C string is dereferenced, after a nil check.
        let answer = unsafe {
            let pool = objc_autoreleasePoolPush();
            let answer = is_agterm(send(shared_workspace(), c"frontmostApplication"));
            objc_autoreleasePoolPop(pool);
            answer
        };
        match answer {
            Some(true) => super::AppFront::Agterm,
            Some(false) => super::AppFront::Other,
            None => super::AppFront::Unreadable,
        }
    }

    /// A block literal's layout, for a block with no captures.
    #[repr(C)]
    struct Block {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: unsafe extern "C" fn(*mut Block, Id),
        descriptor: *const BlockDescriptor,
    }

    #[repr(C)]
    struct BlockDescriptor {
        reserved: usize,
        size: usize,
    }

    /// A block that lives for the whole process: copying it answers the block
    /// itself, and releasing it does nothing.
    const BLOCK_IS_GLOBAL: i32 = 1 << 28;

    /// Called on the main thread for every application activation.
    unsafe extern "C" fn on_activate(_block: *mut Block, note: Id) {
        let pool = objc_autoreleasePoolPush();
        if let Some(agterm) = is_agterm(send1(send(note, c"userInfo"), c"objectForKey:", NSWorkspaceApplicationKey)) {
            super::record_activation(agterm, crate::commands::now_ms());
        }
        objc_autoreleasePoolPop(pool);
    }

    /// Start recording activations, once for the process, and record agterm as
    /// in front since now when it already is. Each of agterm's activations is
    /// sent to `activations`; a later call's sender is dropped, which ends
    /// whatever reads it.
    ///
    /// The observer is registered with no queue, so the block runs on the thread
    /// that posts the notification, which for `NSWorkspace` is the main thread;
    /// the notification center keeps the registration for the life of the
    /// process. The block is built once and leaked, which is what a global block
    /// is.
    ///
    /// What is in front now is recorded only once the observer is registered:
    /// that record is ended only by a notification, so with no observer it would
    /// stand for the life of the process.
    pub fn observe_activation(activations: std::sync::mpsc::Sender<i64>) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = super::ACTIVATIONS.set(activations);
            // SAFETY: as for `agterm_frontmost`. The block and its descriptor are
            // leaked, so they outlive every call the center makes through them,
            // and `on_activate` matches the block's invoke signature.
            unsafe {
                let pool = objc_autoreleasePoolPush();
                let workspace = shared_workspace();
                let descriptor: &'static BlockDescriptor = Box::leak(Box::new(BlockDescriptor { reserved: 0, size: std::mem::size_of::<Block>() }));
                let block: &'static mut Block = Box::leak(Box::new(Block { isa: std::ptr::addr_of!(_NSConcreteGlobalBlock).cast(), flags: BLOCK_IS_GLOBAL, reserved: 0, invoke: on_activate, descriptor }));
                let null = std::ptr::null_mut();
                let token = send4(send(workspace, c"notificationCenter"), c"addObserverForName:object:queue:usingBlock:", NSWorkspaceDidActivateApplicationNotification, null, null, (block as *mut Block).cast());
                let front = (!token.is_null()).then(|| is_agterm(send(workspace, c"frontmostApplication"))).flatten();
                objc_autoreleasePoolPop(pool);
                match front {
                    Some(agterm) => super::record_activation(agterm, crate::commands::now_ms()),
                    None if token.is_null() => tracing::warn!(terminal = "agterm", "could not observe application activations: agterm attention credits neither departures nor input, all refused as foreground_unrecorded"),
                    None => {}
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(fields: Value) -> Value {
        let mut n = serde_json::json!({ "id": "S1", "cwd": "/p/one", "title": "🟢 one", "active": true });
        n.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        n
    }

    #[test]
    fn the_program_in_front_of_a_session_says_what_occupies_it() {
        let of = |argv: Value| occupant(Some(&node(serde_json::json!({ "foreground": argv }))));
        assert_eq!(of(serde_json::json!(["claude", "--continue"])), Occupant::Agent);
        assert_eq!(of(serde_json::json!(["/Users/u/.local/bin/claude"])), Occupant::Agent, "a full path to the agent");
        assert_eq!(of(serde_json::json!(["-zsh"])), Occupant::Shell, "a login shell's argv[0]");
        assert_eq!(of(serde_json::json!(["/bin/zsh", "-il"])), Occupant::Shell);
        // Measured 2026-09-24: a tab that exec'd into ssh reports ssh in front.
        assert_eq!(of(serde_json::json!(["ssh", "-t", "u@host", "cmd"])), Occupant::Other);
        // A shell running something is not a prompt. The Mac's remote tabs hold
        // `claude/remote-session/mac/attach.sh`, which loops over ssh rather than
        // exec'ing into it, so the pane's leader is that shell for the tab's life
        // — and a badged remote title is credited only for `Occupant::Other`.
        assert_eq!(of(serde_json::json!(["/bin/sh", "/Users/u/claude/remote-session/mac/attach.sh", "claude"])), Occupant::Other, "a shell running a script");
        assert_eq!(of(serde_json::json!(["bash", "-c", "deploy"])), Occupant::Other, "a -c command is not a prompt");
        // An npm install runs the agent as a script, and it must not read as a
        // program known not to be the agent, which the verdict refuses outright.
        assert_eq!(of(serde_json::json!(["node", "/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js"])), Occupant::Agent);
        assert_eq!(of(serde_json::json!(["node", "/opt/homebrew/bin/claude", "--continue"])), Occupant::Agent, "through the bin link npm installs");
        assert_eq!(of(serde_json::json!(["node", "server.js"])), Occupant::Other, "another node program");
        assert_eq!(of(serde_json::json!(["node"])), Occupant::Other, "a node prompt");
    }

    #[test]
    fn a_tree_that_does_not_say_leaves_the_occupant_unknown() {
        assert_eq!(occupant(None), Occupant::Unknown("not_in_tree"));
        assert_eq!(occupant(Some(&node(serde_json::json!({})))), Occupant::Unknown("no_foreground"));
        assert_eq!(occupant(Some(&node(serde_json::json!({ "foreground": [] })))), Occupant::Unknown("no_foreground"));
        assert_eq!(occupant(Some(&node(serde_json::json!({ "foreground": "claude" })))), Occupant::Unknown("no_foreground"), "an argv is a list");
        assert_eq!(occupant(Some(&node(serde_json::json!({ "foreground": [""] })))), Occupant::Unknown("no_foreground"), "an empty program name");
    }

    #[test]
    fn an_overlay_terminal_over_a_session_or_a_pane_covers_it() {
        assert_eq!(cover(Some(&node(serde_json::json!({ "overlay": true })))), Cover::Covered);
        assert_eq!(cover(Some(&node(serde_json::json!({ "paneOverlays": ["left"] })))), Cover::Covered);
        assert_eq!(cover(Some(&node(serde_json::json!({ "overlay": false, "paneOverlays": ["left"] })))), Cover::Covered);
        assert_eq!(cover(Some(&node(serde_json::json!({ "overlay": false, "paneOverlays": [] })))), Cover::Clear);
        assert_eq!(cover(Some(&node(serde_json::json!({ "overlay": false })))), Cover::Clear);
        assert_eq!(cover(None), Cover::Unknown);
    }

    /// The node shapes agterm 0.25.0 actually reported either side of a scratch
    /// overlay being toggled, captured 2026-10-03. `overlay` never moves, so
    /// the surfaces array is the only thing that distinguishes them.
    #[test]
    fn a_visible_scratch_surface_covers_the_session_it_is_drawn_over() {
        let shown = node(serde_json::json!({
            "overlay": false,
            "scratch": true,
            "surfaces": [
                { "id": "surface:S1:left", "kind": "left", "visible": false, "active": false },
                { "id": "surface:S1:scratch", "kind": "scratch", "visible": true, "active": true },
            ],
        }));
        assert_eq!(cover(Some(&shown)), Cover::Covered, "agterm reports it through surfaces, not `overlay`");

        // Hiding the scratch leaves its surface in the array — the shell stays
        // alive — so presence cannot be the test or the session would read as
        // covered for the rest of its life.
        let hidden = node(serde_json::json!({
            "overlay": false,
            "scratch": false,
            "surfaces": [
                { "id": "surface:S1:left", "kind": "left", "visible": true, "active": true },
                { "id": "surface:S1:scratch", "kind": "scratch", "visible": false, "active": false },
            ],
        }));
        assert_eq!(cover(Some(&hidden)), Cover::Clear);

        // Before a scratch has ever been opened the array holds only the pane.
        let never = node(serde_json::json!({
            "overlay": false,
            "scratch": false,
            "surfaces": [{ "id": "surface:S1:left", "kind": "left", "visible": true, "active": true }],
        }));
        assert_eq!(cover(Some(&never)), Cover::Clear);
    }

    #[test]
    fn a_node_silent_about_the_session_wide_overlay_is_clear() {
        // agterm's key for it is not measured, and refusing every node without
        // it refused all of agterm's input.
        assert_eq!(cover(Some(&node(serde_json::json!({})))), Cover::Clear);
        assert_eq!(cover(Some(&node(serde_json::json!({ "paneOverlays": [] })))), Cover::Clear);
        assert_eq!(cover(Some(&node(serde_json::json!({ "overlay": "yes" })))), Cover::Clear, "only an explicit true covers");
    }

    #[test]
    fn the_active_window_is_the_open_one_marked_active() {
        let list = serde_json::json!({ "ok": true, "result": { "windows": [
            { "id": "W1", "open": true, "active": false },
            { "id": "W2", "open": true, "active": true },
            { "id": "W3", "open": false, "active": true },
        ] } });
        assert_eq!(active_window(&list).as_deref(), Some("W2"));
        assert_eq!(active_window(&serde_json::json!({ "ok": true, "result": { "windows": [] } })), None);
    }

    fn listed(active: Option<&str>) -> Windows {
        Windows::Listed { active: active.map(str::to_string) }
    }

    #[test]
    fn a_window_is_in_front_only_when_agterm_is_and_the_window_is_its_active_one() {
        assert_eq!(front(AppFront::Agterm, &listed(Some("W1")), "W1"), Front::Yes);
        assert_eq!(front(AppFront::Agterm, &listed(Some("W2")), "W1"), Front::No, "another agterm window");
        assert_eq!(front(AppFront::Other, &listed(Some("W1")), "W1"), Front::No, "another application");
        assert_eq!(front(AppFront::Unreadable, &listed(Some("W1")), "W1"), Front::Unknown("frontmost_unreadable"));
        assert_eq!(front(AppFront::Agterm, &Windows::Unanswered, "W1"), Front::Unknown("window_list_unanswered"));
        assert_eq!(front(AppFront::Agterm, &listed(None), "W1"), Front::Unknown("no_active_window"));
    }

    #[test]
    fn a_window_list_is_read_into_its_active_window() {
        let list = serde_json::json!({ "ok": true, "result": { "windows": [
            { "id": "W1", "open": true, "active": true },
            { "id": "W2", "open": false, "active": false },
        ] } });
        assert_eq!(windows(Some(&list)), listed(Some("W1")));
        assert_eq!(windows(None), Windows::Unanswered);
    }

    #[test]
    fn agterm_is_recognized_by_its_bundle_directory() {
        assert!(is_agterm_bundle("/Applications/agterm.app"));
        assert!(is_agterm_bundle("/Users/u/build/AGTERM.app/"));
        assert!(!is_agterm_bundle("/Applications/Ghostty.app"));
        assert!(!is_agterm_bundle("/Applications/agterm.app/Contents/MacOS/agtermctl"));
    }

    #[test]
    fn an_activation_record_holds_agterm_from_its_activation_until_another_application_takes_over() {
        let mut a = Activation::default();
        assert_eq!(a.since(), Since::Unrecorded, "nothing seen yet");
        a.activated(true, 1_000);
        assert_eq!(a.since(), Since::At(1_000));
        a.activated(false, 2_000);
        assert_eq!(a.since(), Since::Unrecorded, "another application in front");
        a.activated(true, 3_000);
        assert_eq!(a.since(), Since::At(3_000), "the return is a new stretch");
    }
}
