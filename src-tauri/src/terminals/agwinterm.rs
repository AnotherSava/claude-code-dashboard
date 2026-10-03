//! agwinterm: a Windows terminal whose sidebar names each session, reached over
//! its control pipe.
//!
//! This adapter answers two questions, labelling and attention.
//!
//! **Labelling** reads every open window's `tree` for each session's title and
//! context, and writes a row's task as the session's context with `session
//! context`. The name is not written: the sidebar follows the focused pane's
//! program title, which for an agent is the console title this dashboard already
//! writes. The `tree`'s `title` is a field of the fork build; a build without it
//! reports no title, so no session is joined to a row and nothing is written. The
//! protocol itself is in [`super::agwinterm_wire`]; this module owns the pipe.
//!
//! **Attention** comes from the state file agwinterm saves per window, not from
//! the pipe, because the file is where a session switch shows up as an edge: the
//! pipe has no event for it, the window caption never changes, and UI Automation
//! exposes no selection. [`AgwintermAdapter::watch`] diffs the files'
//! selections as they are rewritten, and the rules about the files are pure
//! functions in [`super::agwinterm_state`]. Whether a person made a switch, or
//! typed into a session, is not decided here: this adapter gathers the facts and
//! `crate::attention` applies [`super::person_verdict`], the rule every terminal
//! shares. What it gathers, and why each is read where it is:
//!
//! - **A session is named by its own title alone**, the `tree`'s title for its
//!   focused pane, never by its directory or its sidebar name, so only a console
//!   title this dashboard wrote can name a row. The tree is asked for it when a
//!   departure or input is reported. See the state module.
//! - **A switch is not always a person.** A pipe `session select`, closing a
//!   session and creating one selected all write the same field a click does, so
//!   a departure carries whether a library window of this agwinterm instance
//!   holds the foreground, since when the foreground hook saw it take it, and the
//!   desktop's last input, for the verdict to judge at the switch rather than
//!   when its write is read. "Of this instance" is the window's caption, which is
//!   the instance's app id: the quick terminal and a second build's windows share
//!   the class and are someone else's input. What the facts cannot show is a
//!   scripted select while the user is typing in that same window.
//! - **Each window has its own file**, so the window that switched has to be the
//!   one in front, which is asked of agwinterm's `window.list` rather than of its
//!   window index file: the index is saved through the same settling writer and
//!   can lag or lose a save the same way. A pipe that does not answer is reported
//!   as the front unknown rather than as a switch nobody made.
//! - **A session that is only a shell is not the agent.** A shell left in a pane
//!   after its agent exited keeps the agent's last title until something
//!   rewrites it, so agwinterm's `tree` is asked what runs in the session, and a
//!   session whose every pane is an idle shell is refused whatever its title. The
//!   tree spells "a shell with a child process" and "a root that is no shell"
//!   alike, so a session whose profile launches something other than a shell, a
//!   WSL profile for one, is reported unknown (`unrecognized_root`), and so is
//!   one the tree does not answer for. Since the observation is named by the
//!   session's own title, the verdict does not ask an unknown occupant, which is
//!   what credits an agent running under WSL and tmux.
//! - **A write can be late, and a save can be lost.** The departure is stamped
//!   at the earliest the switch can have been: one settle before its write,
//!   floored when the writer was seen held up by a stuck rename (see
//!   [`super::agwinterm_state::Writer`]), and never later than the newest moment
//!   the session left was known to be selected — the window's previous write,
//!   or a later pipe read. That bound is what covers a save whose rename failed,
//!   which agwinterm drops: the switch first reaches the disk with whatever save
//!   comes next, which can be long after it. The verdict's activation rule reads
//!   the same
//!   earliest bound, so [`check_activations`] asks the pipe
//!   [`super::ACTIVATION_MS`] after each activation whether the window's file
//!   still matches its live selection: a match moves the bound past the
//!   activation, and a mismatch forgets the file's tracking, so the click that
//!   brought the window forward cannot pass for a switch made later. The facts
//!   about a switch are read when its write arrives, and a write placed further
//!   back than a settle and a stall — a held writer's — reports them unknown.
//! - **Input goes to the live selection only.** An input observation carries
//!   whether agwinterm's `tree` confirms the session the file says is selected is
//!   the one selected now (a file found behind the screen is forgotten, so its
//!   late write departs nothing), when that selection began, and whether an
//!   overlay covers it. The scratch cover is not in the tree, so typing into a
//!   session's scratch pane is credited to the session.
//!
//! A visit shorter than agwinterm's 200 ms save settle writes nothing and is not
//! seen.
//!
//! **Reading the files can cost agwinterm a save.** A rename over a file fails
//! while any handle to it is open, sharing delete access included, and
//! agwinterm's writer drops a save whose rename fails until the next save comes.
//! A pass opens each file only for its metadata unless its write time moved, and
//! reads the text of the ones that did, so the window is short but not zero. A
//! save lost to it reaches the watch with the next save, stamped no later than
//! the write before it, and agwinterm's crash restore loses it meanwhile. Only
//! agwinterm can close this, by retrying the rename or renaming with POSIX
//! semantics.
//!
//! **Not a restore witness.** [`TerminalAdapter::sessions`] answers `None`, which
//! the composite reads as "does not answer this question", so restore keeps
//! reading console titles alone. Restore brings back only a session Claude Code's
//! registry vouches for, and for each of those the console adapter already reads
//! the title from the session's own console; the `tree`'s title is that same
//! console title forwarded, so answering would add only a second, possibly older
//! copy of a reading restore already has.
//!
//! **State is held by the instance, not statics.** `terminal_title::sync` and the
//! stale-tab alert build a throwaway adapter on every call, but they only ask
//! `attached_surface` and `stale_remedy`, which this adapter leaves at their
//! defaults; the only caller of `label_targets` and `write_label` is `labels`'
//! one long-lived worker, and the only caller of `watch` and `poll` is
//! `attention`'s. Constructing one does no I/O. The one static is the foreground
//! hook's, which a callback with no state of its own has to write somewhere.
//!
//! **Every read and write has a deadline.** agwinterm serves the pipe with no
//! buffer of its own, so a write completes only once it reads, and a frozen
//! agwinterm would hold a plain write forever. The pipe is opened for
//! overlapped I/O, each read and write waits on its own event for what is left
//! of the deadline, and an operation still pending then is cancelled and waited
//! out before its buffer goes away.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::agwinterm_state::{self as state, Frontmost, Watch, TERMINAL as NAME};
use super::agwinterm_wire as wire;
use super::{Front, LabelTarget, LabelWrite, LastInput, Observation, Selection, TerminalAdapter, TerminalSession};

/// How long `ping`, `window.list` and `tree` get to answer. `tree` is served on
/// the pipe thread without a hop to the UI, measured at 12 ms.
const READ_DEADLINE: Duration = Duration::from_secs(3);

/// How long `session context` gets. It queues on the window's UI thread, and
/// agwinterm answers `ok:false` itself after 15 s, so waiting a little longer
/// lets its own refusal arrive rather than ours.
const WRITE_DEADLINE: Duration = Duration::from_secs(17);

/// How long after a failed connect the pipe is left alone. A pass that could not
/// look is retried by the worker anyway, so this only stops a terminal that is
/// not running from being probed more often than that.
const BACKOFF: Duration = Duration::from_secs(10);

/// How long a connect waits for a listening instance when every one is taken.
/// agwinterm creates the next instance as soon as it accepts one, and its own
/// hooks connect once per hook event, so a busy pipe is a gap of milliseconds.
const BUSY_WAIT_MS: u32 = 500;

/// The longest reply line accepted. A `tree` of every session in a large window
/// is tens of kilobytes; anything that streams past this without a newline is
/// not an agwinterm reply.
const MAX_REPLY_BYTES: usize = 4 << 20;

/// How much one read asks for.
const READ_CHUNK: usize = 64 << 10;

const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_IO_PENDING: i32 = 997;
const WAIT_OBJECT_0: u32 = 0;

pub struct AgwintermAdapter {
    shared: Arc<Shared>,
}

/// What the adapter and its watch thread share.
struct Shared {
    pipe: Mutex<Pipe>,
    /// The instance's app id, which names its data directory and is the caption
    /// of each of its library windows.
    app_id: String,
    /// agwinterm's data directory, `%LOCALAPPDATA%\<app id>`, or `None` when
    /// `LOCALAPPDATA` is unset.
    app_dir: Option<PathBuf>,
    /// Each window's tracking. Written by the watch, read by
    /// [`poll`](TerminalAdapter::poll), which needs to know which session is on
    /// screen to credit input to it, and forgotten by the poll when agwinterm
    /// says the file is behind the screen.
    watch: Mutex<Watch>,
    /// Whether the selection watch is in place, once it has been started.
    watching: OnceLock<Arc<AtomicBool>>,
}

impl AgwintermAdapter {
    /// Reads `AGWINTERM_PIPE`, `AGWINTERM_APP_ID` and `LOCALAPPDATA`, and does no
    /// I/O. The first two are set only in processes agwinterm starts, so a
    /// dashboard started at login uses the release build's pipe and data.
    pub fn new() -> Self {
        let path = wire::pipe_path(std::env::var("AGWINTERM_PIPE").ok().as_deref());
        let app_id = state::app_id(std::env::var("AGWINTERM_APP_ID").ok().as_deref());
        let app_dir = std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(&app_id));
        let pipe = Mutex::new(Pipe { path, conn: None, down: None, link: None });
        Self { shared: Arc::new(Shared { pipe, app_id, app_dir, watch: Mutex::new(Watch::default()), watching: OnceLock::new() }) }
    }
}

struct Pipe {
    path: String,
    conn: Option<Conn>,
    /// Until when connecting is not retried, and whether the failure was that no
    /// agwinterm is listening at all, which is an answer rather than a failure.
    /// An `Instant`, so a wall clock set back cannot stretch the wait.
    down: Option<(Instant, bool)>,
    /// The last `agwinterm_link` outcome logged, so the line marks transitions.
    link: Option<&'static str>,
}

struct Conn {
    /// Opened for overlapped I/O, so it is only ever read and written through
    /// [`overlapped`]: std's own `read` and `write` abort the process on an
    /// operation that does not complete at once.
    file: File,
    /// Bytes read and not yet returned as a line, since a reply arrives in pieces.
    buf: Vec<u8>,
}

impl Conn {
    fn new(file: File) -> Self {
        Conn { file, buf: Vec::new() }
    }
}

/// Why a call produced no answer.
#[derive(Debug)]
enum Fail {
    /// Nothing listens on the pipe: agwinterm is not running.
    Absent,
    /// The pipe exists and would not take us, or the connection broke.
    Unreachable(String),
    Timeout,
    /// Something answered on the pipe and did not say it was agwinterm.
    BadIdentity(String),
    /// agwinterm answered `ok:false`.
    Refused(String),
    /// Inside the backoff after a failed connect.
    Backoff { absent: bool },
}

impl Fail {
    fn reason(&self) -> String {
        match self {
            Fail::Absent => "agwinterm is not running".to_string(),
            Fail::Unreachable(e) => format!("agwinterm's pipe could not be used: {e}"),
            Fail::Timeout => "agwinterm did not answer in time".to_string(),
            Fail::BadIdentity(r) => format!("the pipe answered as something other than agwinterm: {r}"),
            Fail::Refused(e) => e.clone(),
            Fail::Backoff { .. } => "agwinterm was unreachable a moment ago; not retrying yet".to_string(),
        }
    }
}

impl Pipe {
    /// Log the link's state when it changes.
    fn set_link(&mut self, outcome: &'static str, detail: &str) {
        if self.link == Some(outcome) {
            return;
        }
        self.link = Some(outcome);
        match outcome {
            "bad_identity" => tracing::warn!(decision = "agwinterm_link", terminal = NAME, outcome, detail, path = %self.path, "agwinterm link"),
            _ => tracing::info!(decision = "agwinterm_link", terminal = NAME, outcome, detail, path = %self.path, "agwinterm link"),
        }
    }

    fn connect(&mut self) -> Result<(), Fail> {
        let now = Instant::now();
        if let Some((until, absent)) = self.down {
            if now < until {
                return Err(Fail::Backoff { absent });
            }
        }
        let fail = match self.open() {
            Ok((conn, version)) => {
                self.conn = Some(conn);
                self.down = None;
                self.set_link("connected", &version);
                return Ok(());
            }
            Err(f) => f,
        };
        self.down = Some((now + BACKOFF, matches!(fail, Fail::Absent)));
        let outcome = match &fail {
            Fail::Absent => "absent",
            Fail::Timeout => "timeout",
            Fail::BadIdentity(_) => "bad_identity",
            _ => "unreachable",
        };
        self.set_link(outcome, &fail.reason());
        Err(fail)
    }

    /// Open the pipe and check that agwinterm is what answers on it. The pipe
    /// takes the default ACL, so whatever created the name first is answered.
    fn open(&self) -> Result<(Conn, String), Fail> {
        let mut conn = Conn::new(open_pipe(&self.path)?);
        let line = exchange(&mut conn, &wire::ping_req(), READ_DEADLINE)?;
        match wire::parse_reply(&line) {
            Ok(v) => match v.as_str().filter(|s| s.starts_with("agwinterm ")) {
                Some(version) => Ok((conn, version.to_string())),
                None => Err(Fail::BadIdentity(line)),
            },
            Err(e) => Err(Fail::BadIdentity(e)),
        }
    }

    /// One request and its reply.
    ///
    /// A timeout, a broken pipe or an `ok:false` drops the connection, and it is
    /// never reused: a reply that was abandoned would otherwise be read as the
    /// answer to the next request.
    fn call(&mut self, req: &str, deadline: Duration) -> Result<serde_json::Value, Fail> {
        if self.conn.is_none() {
            self.connect()?;
        }
        let line = match exchange(self.conn.as_mut().expect("connected just above"), req, deadline) {
            Ok(line) => line,
            Err(f) => {
                // A connection that stops answering, or one agwinterm closed by
                // restarting, is a link change worth one line; a refusal is not.
                self.set_link(if matches!(f, Fail::Timeout) { "timeout" } else { "unreachable" }, &f.reason());
                self.conn = None;
                return Err(f);
            }
        };
        let result = wire::parse_reply(&line).map_err(Fail::Refused);
        if result.is_err() {
            self.conn = None;
        }
        result
    }

    fn targets(&mut self) -> Result<Vec<LabelTarget>, Fail> {
        let windows = wire::parse_windows(&self.call(&wire::windows_req(), READ_DEADLINE)?);
        let mut targets = Vec::new();
        for w in &windows {
            targets.extend(wire::targets_from(w, &wire::parse_tree(&self.call(&wire::tree_req(w), READ_DEADLINE)?)));
        }
        // A successful read after a timeout on an established connection is the
        // link working again.
        self.set_link("connected", "");
        Ok(targets)
    }
}

/// Open the pipe for overlapped I/O.
///
/// Every instance being taken (`ERROR_PIPE_BUSY`) is waited out briefly and
/// retried once rather than read as a dead pipe: agwinterm has a gap between
/// accepting one client and listening for the next, and its own hooks connect
/// once per hook event.
fn open_pipe(path: &str) -> Result<File, Fail> {
    let open = || std::fs::OpenOptions::new().read(true).write(true).custom_flags(FILE_FLAG_OVERLAPPED).open(path);
    let opened = match open() {
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated wide string that outlives the call. The
            // result is not needed: a wait that timed out leaves the retry to
            // fail with the same error, which is then reported.
            unsafe { WaitNamedPipeW(wide.as_ptr(), BUSY_WAIT_MS) };
            open()
        }
        other => other,
    };
    opened.map_err(|e| match e.raw_os_error() {
        // No pipe of that name exists.
        Some(ERROR_FILE_NOT_FOUND) => Fail::Absent,
        _ => Fail::Unreachable(e.to_string()),
    })
}

/// Write one request line and read one reply line, all of it inside `deadline`.
///
/// The deadline is checked on every pass, not only when nothing has arrived, so
/// a peer that keeps sending bytes without ever ending the line is cut off too,
/// and the line is refused once it passes [`MAX_REPLY_BYTES`].
fn exchange(conn: &mut Conn, req: &str, deadline: Duration) -> Result<String, Fail> {
    let until = Instant::now() + deadline;
    let handle = conn.file.as_raw_handle() as isize;
    let line = format!("{req}\n");
    let mut sent = 0;
    while sent < line.len() {
        let rest = &line.as_bytes()[sent..];
        // SAFETY: `rest` outlives the call, which does not return while the
        // operation is pending.
        sent += unsafe { overlapped(handle, Op::Write, rest.as_ptr() as *mut u8, rest.len(), until) }?;
    }
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        if let Some(end) = conn.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = conn.buf.drain(..=end).collect();
            return Ok(String::from_utf8_lossy(&line[..end]).trim_end_matches('\r').to_string());
        }
        if conn.buf.len() > MAX_REPLY_BYTES {
            return Err(Fail::Unreachable(format!("a reply ran past {MAX_REPLY_BYTES} bytes without ending its line")));
        }
        if Instant::now() >= until {
            return Err(Fail::Timeout);
        }
        // SAFETY: as for the write above.
        let n = unsafe { overlapped(handle, Op::Read, chunk.as_mut_ptr(), chunk.len(), until) }?;
        if n == 0 {
            return Err(Fail::Unreachable("the pipe closed".to_string()));
        }
        conn.buf.extend_from_slice(&chunk[..n]);
    }
}

#[derive(Clone, Copy)]
enum Op {
    Read,
    Write,
}

/// One overlapped read or write, waited on until `until` and cancelled past it.
///
/// # Safety
///
/// `handle` is a pipe opened with `FILE_FLAG_OVERLAPPED`, and `buf` is valid for
/// `len` bytes (writable for a read) for the duration of the call. The call does
/// not return while the operation is pending, since a cancelled operation is
/// waited out too.
unsafe fn overlapped(handle: isize, op: Op, buf: *mut u8, len: usize, until: Instant) -> Result<usize, Fail> {
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
    if event == 0 {
        return Err(Fail::Unreachable(std::io::Error::last_os_error().to_string()));
    }
    let _event = OwnedHandle::from_raw_handle(event as *mut c_void);
    let mut ov = Overlapped { event, ..Overlapped::default() };
    let started = match op {
        Op::Read => ReadFile(handle, buf, len, std::ptr::null_mut(), &mut ov),
        Op::Write => WriteFile(handle, buf, len, std::ptr::null_mut(), &mut ov),
    };
    if started == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_IO_PENDING) {
            return Err(Fail::Unreachable(e.to_string()));
        }
    }
    let left = until.saturating_duration_since(Instant::now()).as_millis().min(u128::from(u32::MAX - 1)) as u32;
    let signalled = WaitForSingleObject(event, left) == WAIT_OBJECT_0;
    if !signalled {
        CancelIoEx(handle, &ov);
    }
    let mut n: u32 = 0;
    // Waits for the cancellation to land as well, so `ov` and `buf` outlive the
    // operation. One that completed in the race is taken as it is.
    if GetOverlappedResult(handle, &ov, &mut n, 1) != 0 {
        return Ok(n as usize);
    }
    if !signalled {
        return Err(Fail::Timeout);
    }
    Err(Fail::Unreachable(std::io::Error::last_os_error().to_string()))
}

impl TerminalAdapter for AgwintermAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    /// `None`: this terminal does not answer the restore question. See the module
    /// doc for why.
    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        None
    }

    /// Input to the session on screen, the secondary signal. Departures come
    /// only from [`watch`](TerminalAdapter::watch): `crate::attention` applies a
    /// watched observation the moment it arrives, so the poll is not needed for
    /// correctness, and sampling `tree`'s `active` flag for departures would only
    /// find the same switches later, missing every visit between two ticks.
    ///
    /// The input is the desktop's, offered to the committed selection of the
    /// window agwinterm says is in front, with whether a library window of this
    /// instance holds the foreground and since when; [`state::input_observation`]
    /// adds what agwinterm's `tree` says of that session now. A selection the
    /// tree says is behind the screen also has its tracking forgotten here, since
    /// that is a fact about the file whatever is decided about the input.
    fn poll(&mut self, now_ms: i64) -> Vec<Observation> {
        let s = &self.shared;
        let skip = |outcome: &'static str, why: &str| {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome, "{why}");
            Vec::new()
        };
        if !s.watching.get().is_some_and(|w| w.load(Ordering::Relaxed)) {
            return skip("no_watch", "the selection watch is not running, so which session is on screen is not known");
        }
        let Some(idle) = crate::idle::idle_ms() else {
            return skip("no_input_clock", "the desktop's input clock could not be read");
        };
        // SAFETY: no arguments; answers a handle or zero.
        let foreground = unsafe { GetForegroundWindow() };
        let in_front = if is_library_window(foreground, &s.app_id) { Front::Yes } else { Front::No };
        let front_since = super::windows::front_since(*HELD.lock().unwrap(), foreground);
        let (frontmost, tree) = {
            let mut pipe = s.pipe.lock().unwrap();
            let frontmost = front(&mut pipe);
            let tree = frontmost.window().and_then(|w| pipe.call(&wire::tree_req(w), READ_DEADLINE).ok());
            (frontmost, tree)
        };
        // Not `front_unknown`, which is the verdict's refusal of an observation;
        // here there is no observation to judge.
        let Some(window) = frontmost.window() else {
            return skip("no_front_window", "agwinterm could not say which window is in front");
        };
        let Some(on_screen) = state::on_screen(s.watch.lock().unwrap().files.tracked(), window).cloned() else {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome = "no_selection", window, "the window in front has no settled selection");
            return Vec::new();
        };
        let root_is_shell = s.app_dir.as_deref().is_some_and(|d| state::root_is_shell(on_screen.entry.profile.as_deref(), profiles(d).as_ref()));
        let observation = state::input_observation(&on_screen, tree.as_ref(), root_is_shell, in_front, front_since, now_ms - idle as i64);
        if matches!(observation.kind, super::ObservationKind::Input(f) if f.selection == Selection::Lagging) {
            s.watch.lock().unwrap().forget_lagging(window, &on_screen.committed);
        }
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "poll", outcome = "input", window, title = ?observation.session.title, since_ms = on_screen.since_ms, "agwinterm poll");
        vec![observation]
    }

    /// Report a departure the moment a window's selection changes on disk.
    ///
    /// The foreground hook starts here whatever the directory holds, and the
    /// watch waits for `windows\` if agwinterm has not created it yet, so a
    /// terminal installed or first run while the dashboard is up is picked up
    /// without a restart.
    fn watch(&self, sink: Sender<Observation>) {
        let Some(app_dir) = self.shared.app_dir.clone() else {
            tracing::warn!(terminal = NAME, "no LOCALAPPDATA; the selection watcher cannot start");
            return;
        };
        let (activations, rx) = std::sync::mpsc::channel();
        if ACTIVATIONS.set(activations).is_ok() {
            let shared = self.shared.clone();
            std::thread::spawn(move || check_activations(&shared, &rx));
        }
        watch_foreground(&self.shared.app_id);
        let shared = self.shared.clone();
        let watching = super::snapshot_watch::spawn(NAME, app_dir.join("windows"), move |dir, baseline| reread(dir, baseline, &shared), sink);
        let _ = self.shared.watching.set(watching);
    }

    fn can_label(&self) -> bool {
        true
    }

    /// Every session in every open window. `Some(vec![])` when agwinterm is not
    /// running, since that is an answer; `None` when it is running and could not
    /// be read.
    fn label_targets(&self) -> Option<Vec<LabelTarget>> {
        let mut pipe = self.shared.pipe.lock().unwrap();
        match pipe.targets() {
            Ok(targets) => Some(targets),
            Err(Fail::Absent | Fail::Backoff { absent: true }) => Some(Vec::new()),
            Err(_) => None,
        }
    }

    fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
        let (window, target) = wire::split_key(key).ok_or_else(|| format!("label key {key:?} is not a window and a session"))?;
        let req = match write {
            LabelWrite::Context(text) => wire::context_req(window, target, text),
            LabelWrite::ClearContext => wire::clear_context_req(window, target),
        };
        self.shared.pipe.lock().unwrap().call(&req, WRITE_DEADLINE).map(|_| ()).map_err(|f| f.reason())
    }
}

/// Re-read the window files, and report every departure with the facts the
/// person verdict judges it by.
///
/// Every decision about the files is [`Watch::pass`]'s, and every decision about
/// a person is the verdict's, made in `crate::attention`; this owns the reads,
/// the pipe and the log. Only files that changed are logged, one line each, so a
/// watch that runs and finds nothing still reads differently from one that never
/// ran without a line for every window on every save.
fn reread(dir: &Path, baseline: bool, shared: &Shared) -> Vec<Observation> {
    // A baseline starts from nothing, so it reads every file.
    let known = if baseline { HashMap::new() } else { shared.watch.lock().unwrap().files.written().clone() };
    let Some(files) = super::window_files::read_changed(dir, &known) else {
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "watch", outcome = "unreadable_dir", dir = %dir.display(), "the window state directory could not be listed");
        return Vec::new();
    };
    let changed = shared.watch.lock().unwrap().pass(files, baseline, crate::commands::now_ms());
    // The facts about a person, read once per pass so every departure in it is
    // judged against the same instant, and only when there is a departure.
    let facts = changed.iter().any(|c| c.departure.is_some()).then(|| {
        // SAFETY: no arguments; answers a handle or zero.
        let foreground = unsafe { GetForegroundWindow() };
        let in_front = is_library_window(foreground, &shared.app_id);
        let front_since = super::windows::front_since(*HELD.lock().unwrap(), foreground);
        let now_ms = crate::commands::now_ms();
        let last_input = crate::idle::idle_ms().map_or(LastInput::Unknown, |idle| LastInput::At(now_ms - idle as i64));
        let frontmost = front(&mut shared.pipe.lock().unwrap());
        (in_front, front_since, last_input, frontmost, now_ms)
    });
    let mut profile_list: Option<Option<serde_json::Value>> = None;
    let mut out = Vec::new();
    for c in changed {
        let (Some(d), Some((in_front, front_since, last_input, frontmost, read_ms))) = (c.departure, facts.as_ref()) else {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "watch", outcome = c.outcome, window = %c.window, "agwinterm window file rewritten");
            continue;
        };
        let tree = shared.pipe.lock().unwrap().call(&wire::tree_req(&c.window), READ_DEADLINE).ok();
        let profiles = profile_list.get_or_insert_with(|| shared.app_dir.as_deref().and_then(profiles));
        let root_is_shell = state::root_is_shell(d.entry.profile.as_deref(), profiles.as_ref());
        let front = state::front_of(&c.window, *in_front, frontmost);
        let observation = d.observation(tree.as_ref(), root_is_shell, front, *front_since, *last_input, *read_ms);
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "watch", outcome = c.outcome, window = %c.window, front = ?front, front_since = ?front_since, frontmost = ?frontmost, last_input = ?last_input, occupant = ?observation.occupant, departed = ?observation.session.title, at_ms = d.at_ms, switched_ms = d.switched_ms, "agwinterm window file rewritten");
        out.push(observation);
    }
    out
}

/// The window agwinterm says it activated last, from `window.list`, which
/// answers from agwinterm's memory rather than from its index file.
fn front(pipe: &mut Pipe) -> Frontmost {
    Frontmost::from_list(pipe.call(&wire::windows_req(), READ_DEADLINE).ok().as_ref())
}

/// agwinterm's `profiles.json`, or `None` when it cannot be read.
fn profiles(app_dir: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(app_dir.join("profiles.json")).ok().and_then(|text| serde_json::from_str(&text).ok())
}

/// The library window of the watched instance that last took the foreground,
/// and when.
///
/// A static because it is written from a `WINEVENTPROC`, a bare
/// `extern "system" fn` with nowhere else to put it. Never cleared when focus
/// moves elsewhere, because every reader asks only about the window in the
/// foreground now; what this adds is *since when*, which is what stops a stale
/// input clock crediting typing in another program to the session on screen.
///
/// Written only from events the hook delivered, never seeded when the hook
/// starts. A seed is renewed or ended only by events, and an elevated agwinterm
/// delivers none to this process, so one would stand for the life of the
/// process. The cost is that a window already in front when the dashboard starts
/// is judged only once focus has moved to it again.
static HELD: Mutex<Option<(isize, i64)>> = Mutex::new(None);

/// Where the foreground hook reports when a library window took the foreground,
/// for [`check_activations`].
static ACTIVATIONS: OnceLock<Sender<i64>> = OnceLock::new();

/// For each activation of a library window, ask agwinterm, once
/// [`super::ACTIVATION_MS`] has passed, whether the window in front still has
/// the selection its file says, and tell the watch what it said
/// ([`Watch::confirm_live`](state::Watch::confirm_live)).
///
/// The question the verdict's activation rule cannot otherwise answer for this
/// terminal: whether a switch reported later was made after the window came
/// forward, or was the click that brought it forward and only reached the disk
/// late. The request instant is what is recorded, which is no later than the
/// pipe's reading of it. A pipe that does not answer leaves the tracking as it
/// was, and the activation rule then refuses the next switch as possibly the
/// activating click.
fn check_activations(shared: &Shared, rx: &std::sync::mpsc::Receiver<i64>) {
    while let Ok(activated_at) = rx.recv() {
        let wait = activated_at + super::ACTIVATION_MS + 1 - crate::commands::now_ms();
        if wait > 0 {
            std::thread::sleep(Duration::from_millis(wait as u64));
        }
        let mut pipe = shared.pipe.lock().unwrap();
        let Frontmost::Named(window) = front(&mut pipe) else {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "activation", outcome = "no_front_window", "agwinterm could not say which window came forward");
            continue;
        };
        let tracked = shared.watch.lock().unwrap().files.tracked().get(&window).filter(|t| t.left_at.is_none()).map(|t| t.committed.clone());
        let Some(committed) = tracked else { continue };
        let asked_at = crate::commands::now_ms();
        let Ok(tree) = pipe.call(&wire::tree_req(&window), READ_DEADLINE) else {
            tracing::debug!(decision = "attention_poll", terminal = NAME, source = "activation", outcome = "tree_unanswered", window, "agwinterm did not say what is selected in the window that came forward");
            continue;
        };
        drop(pipe);
        let live = wire::view_of(&tree, &committed).is_some_and(|v| v.active);
        shared.watch.lock().unwrap().confirm_live(&window, &committed, live, asked_at);
        tracing::debug!(decision = "attention_poll", terminal = NAME, source = "activation", outcome = if live { "selection_confirmed" } else { "file_behind_screen" }, window, activated_at, asked_at, "agwinterm window came forward");
    }
}

/// The app id the foreground hook recognizes library windows by, set when the
/// hook starts.
static HOOK_APP_ID: OnceLock<String> = OnceLock::new();

/// Start the foreground hook, once for the process.
///
/// Global rather than scoped to agwinterm's process: foreground changes arrive a
/// few times a minute, and a global hook needs no tracking of which pid agwinterm
/// runs as or of it restarting. The thread pumps messages because an
/// out-of-context hook is delivered through the registering thread's queue.
fn watch_foreground(app_id: &str) {
    if HOOK_APP_ID.set(app_id.to_string()).is_err() {
        return;
    }
    std::thread::spawn(|| {
        // SAFETY: plain Win32 calls on this thread. The callback is a `'static`
        // fn, and `msg` outlives every call it is passed to.
        unsafe {
            let hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, 0, on_foreground, 0, 0, WINEVENT_OUTOFCONTEXT);
            if hook == 0 {
                tracing::warn!(terminal = NAME, "could not hook foreground changes: agwinterm attention credits neither departures nor input");
                return;
            }
            let mut msg: super::windows::Msg = std::mem::zeroed();
            loop {
                let got = GetMessageW(&mut msg, 0, 0, 0);
                if got <= 0 {
                    // Nothing will renew the record now, so it is ended rather
                    // than left to speak for a window long after it stopped
                    // being true.
                    *HELD.lock().unwrap() = None;
                    tracing::warn!(terminal = NAME, got, "the foreground hook's message loop ended: agwinterm attention credits neither departures nor input from now on");
                    return;
                }
                DispatchMessageW(&msg);
            }
        }
    });
}

unsafe extern "system" fn on_foreground(_hook: isize, _event: u32, hwnd: isize, id_object: i32, id_child: i32, _thread: u32, _time: u32) {
    if id_object == OBJID_WINDOW && id_child == CHILDID_SELF && HOOK_APP_ID.get().is_some_and(|id| is_library_window(hwnd, id)) {
        let now = crate::commands::now_ms();
        *HELD.lock().unwrap() = Some((hwnd, now));
        if let Some(tx) = ACTIVATIONS.get() {
            let _ = tx.send(now);
        }
    }
}

/// Whether `hwnd` is a library window of the agwinterm instance `app_id` names.
/// See [`state::is_library_window`].
fn is_library_window(hwnd: isize, app_id: &str) -> bool {
    if hwnd == 0 {
        return false;
    }
    // SAFETY: each call writes at most `len` units into the buffer
    // `window_string` lends it, and a stale handle reads as an empty string.
    // `GetWindowTextW` on another process's window reads its stored caption
    // without sending it a message, so a hung agwinterm cannot block this.
    let (class, caption) = unsafe { (super::windows::window_string(|buf, len| GetClassNameW(hwnd, buf, len)), super::windows::window_string(|buf, len| GetWindowTextW(hwnd, buf, len))) };
    state::is_library_window(&class, &caption, app_id)
}

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const OBJID_WINDOW: i32 = 0;
const CHILDID_SELF: i32 = 0;

type WinEventProc = unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32);

// Declared here as well as in `windows`: a second declaration of an import is
// free, and these serve this adapter's own hook.
#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> isize;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn SetWinEventHook(min: u32, max: u32, hmod: isize, cb: WinEventProc, pid: u32, thread: u32, flags: u32) -> isize;
    fn GetMessageW(msg: *mut super::windows::Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn DispatchMessageW(msg: *const super::windows::Msg) -> isize;
}

/// `OVERLAPPED`, with the offset union as its two halves; a pipe ignores both.
#[repr(C)]
#[derive(Default)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: isize,
}

// Declared by hand like the rest of this crate's Win32 calls, rather than
// through the `windows` crate, which only `wt_tabs` uses.
#[link(name = "kernel32")]
extern "system" {
    fn CreateEventW(attributes: *const c_void, manual_reset: i32, initial: i32, name: *const u16) -> isize;
    fn ReadFile(file: isize, buf: *mut u8, len: u32, read: *mut u32, overlapped: *mut Overlapped) -> i32;
    fn WriteFile(file: isize, buf: *const u8, len: u32, written: *mut u32, overlapped: *mut Overlapped) -> i32;
    fn WaitForSingleObject(handle: isize, ms: u32) -> u32;
    fn CancelIoEx(file: isize, overlapped: *const Overlapped) -> i32;
    fn GetOverlappedResult(file: isize, overlapped: *const Overlapped, transferred: *mut u32, wait: i32) -> i32;
    fn WaitNamedPipeW(name: *const u16, ms: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, Read, Write};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    use super::*;

    const PIPE_ACCESS_DUPLEX: u32 = 3;
    const PIPE_TYPE_BYTE_WAIT: u32 = 0;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(name: *const u16, open_mode: u32, pipe_mode: u32, max_instances: u32, out_size: u32, in_size: u32, timeout_ms: u32, attributes: *const c_void) -> isize;
        fn ConnectNamedPipe(pipe: isize, overlapped: *mut Overlapped) -> i32;
    }

    /// A pipe name no other test or process uses.
    fn unique_path() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(r"\\.\pipe\ccdash-agwinterm-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    /// One server instance of `path`, as a synchronous handle, with buffers of
    /// `buffer` bytes each way.
    fn instance(path: &str, buffer: u32) -> File {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let h = unsafe { CreateNamedPipeW(wide.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE_WAIT, PIPE_UNLIMITED_INSTANCES, buffer, buffer, 0, std::ptr::null()) };
        assert!(h != -1 && h != 0, "CreateNamedPipeW: {}", std::io::Error::last_os_error());
        unsafe { File::from_raw_handle(h as *mut c_void) }
    }

    /// Wait for a client on a server instance. An already-connected client
    /// (`ERROR_PIPE_CONNECTED`) is a connection too.
    fn accept(server: &File) {
        let ok = unsafe { ConnectNamedPipe(server.as_raw_handle() as isize, std::ptr::null_mut()) };
        assert!(ok != 0 || std::io::Error::last_os_error().raw_os_error() == Some(535));
    }

    /// Run `exchange` on its own thread and give up waiting after `patience`, so
    /// a test of a deadline fails rather than hanging when the deadline is not
    /// kept.
    fn exchange_within(path: &str, req: String, deadline: Duration, patience: Duration) -> Result<Result<String, Fail>, mpsc::RecvTimeoutError> {
        let mut conn = Conn::new(open_pipe(path).expect("the server instance is listening"));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(exchange(&mut conn, &req, deadline));
        });
        rx.recv_timeout(patience)
    }

    #[test]
    fn a_reply_round_trips() {
        let path = unique_path();
        let mut server = instance(&path, 4096);
        let s = std::thread::spawn(move || {
            accept(&server);
            let mut got = [0u8; 64];
            let n = server.read(&mut got).unwrap();
            server.write_all(b"{\"ok\":true,\"result\":\"agwinterm 1\"}\r\n").unwrap();
            String::from_utf8_lossy(&got[..n]).to_string()
        });
        let reply = exchange_within(&path, wire::ping_req(), READ_DEADLINE, Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(reply, r#"{"ok":true,"result":"agwinterm 1"}"#);
        assert_eq!(s.join().unwrap(), format!("{}\n", wire::ping_req()));
    }

    #[test]
    fn a_peer_streaming_without_a_newline_is_cut_off_at_the_deadline() {
        let path = unique_path();
        let mut server = instance(&path, 4096);
        std::thread::spawn(move || {
            accept(&server);
            let mut got = [0u8; 64];
            let _ = server.read(&mut got);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) && server.write_all(&[b'x'; 64]).is_ok() {}
        });
        let result = exchange_within(&path, wire::ping_req(), Duration::from_millis(200), Duration::from_secs(2)).expect("exchange returned while bytes were still arriving");
        assert!(matches!(result, Err(Fail::Timeout)), "{result:?}");
    }

    #[test]
    fn a_reply_past_the_cap_is_refused() {
        let path = unique_path();
        let mut server = instance(&path, 64 << 10);
        std::thread::spawn(move || {
            accept(&server);
            let mut got = [0u8; 64];
            let _ = server.read(&mut got);
            let block = vec![b'x'; 64 << 10];
            while server.write_all(&block).is_ok() {}
        });
        let result = exchange_within(&path, wire::ping_req(), Duration::from_secs(60), Duration::from_secs(20)).expect("exchange kept reading past the cap");
        assert!(matches!(result, Err(Fail::Unreachable(_))), "{result:?}");
    }

    #[test]
    fn a_write_the_peer_never_reads_times_out() {
        // A peer that accepts and then reads nothing, with no buffer of its own,
        // as a frozen agwinterm would be. The request is larger than any buffer
        // the system might grant anyway.
        let path = unique_path();
        let server = instance(&path, 0);
        std::thread::spawn(move || {
            accept(&server);
            std::thread::sleep(Duration::from_secs(4));
            drop(server);
        });
        let started = Instant::now();
        let result = exchange_within(&path, "x".repeat(1 << 20), Duration::from_millis(200), Duration::from_secs(2)).expect("the write blocked past its deadline");
        assert!(matches!(result, Err(Fail::Timeout)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_busy_pipe_is_waited_for_rather_than_reported_dead() {
        // The only instance is taken by another client; the server listens again
        // shortly afterwards, as agwinterm does after accepting.
        let path = unique_path();
        let first = instance(&path, 4096);
        let _holder = open_pipe(&path).expect("first client");
        accept(&first);
        let p = path.clone();
        let s = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let next = instance(&p, 4096);
            accept(&next);
            next
        });
        assert!(open_pipe(&path).is_ok(), "a busy pipe read as unreachable");
        drop(s.join());
    }

    #[test]
    fn a_missing_pipe_is_absent() {
        assert!(matches!(open_pipe(&unique_path()), Err(Fail::Absent)));
    }

    /// Serve one client on `path`, answering each request line with the next of
    /// `replies`, and hand back the requests as received.
    fn serve(path: &str, replies: Vec<String>) -> std::thread::JoinHandle<Vec<String>> {
        let server = instance(path, 64 << 10);
        std::thread::spawn(move || {
            accept(&server);
            let mut lines = std::io::BufReader::new(&server).lines();
            let mut seen = Vec::new();
            for reply in replies {
                seen.push(lines.next().expect("a request").expect("readable"));
                (&server).write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
            seen
        })
    }

    const PING: &str = r#"{"ok":true,"result":"agwinterm 0.20.14.1"}"#;

    fn pipe(path: &str) -> Pipe {
        Pipe { path: path.to_string(), conn: None, down: None, link: None }
    }

    #[test]
    fn a_labels_read_lists_every_session_of_every_open_window() {
        let path = unique_path();
        let tree = r#"{"ok":true,"result":{"workspaces":[{"sessions":[{"id":"s1","name":"session 1","title":"🔵 dash","context":"Fix the build"}]}]}}"#;
        let s = serve(&path, vec![PING.to_string(), r#"{"ok":true,"result":{"windows":[{"id":"w1","open":true,"active":true},{"id":"w2","open":false}]}}"#.to_string(), tree.to_string()]);
        let targets = pipe(&path).targets().unwrap();
        assert_eq!(targets, vec![LabelTarget { key: "w1/s1".into(), title: Some("🔵 dash".into()), context: Some("Fix the build".into()), max_utf16: wire::CONTEXT_MAX_UTF16 }]);
        assert_eq!(s.join().unwrap().len(), 3, "a ping, the window list and one tree: a closed window is not asked");
    }

    #[test]
    fn a_refusal_is_a_failure_and_drops_the_connection() {
        let path = unique_path();
        let s = serve(&path, vec![PING.to_string(), r#"{"ok":false,"error":"session not found; nothing changed"}"#.to_string()]);
        let mut p = pipe(&path);
        assert!(matches!(p.call(&wire::clear_context_req("w1", "s1"), WRITE_DEADLINE), Err(Fail::Refused(e)) if e == "session not found; nothing changed"));
        assert!(p.conn.is_none());
        drop(s.join());
    }
}
