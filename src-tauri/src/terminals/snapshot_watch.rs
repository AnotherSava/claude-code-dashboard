//! Watching a terminal's own state directory for the moment its selection
//! changes.
//!
//! Shared by every adapter whose terminal saves which session is selected to a
//! file per window, which today is agterm on macOS and agwinterm on Windows. Both
//! save the same way, and the three things a watch has to get right come from
//! *how* they save rather than from what the file holds, which is why one loop
//! serves both and only the reading is the adapter's:
//!
//! - **Watch the directory, not the files.** Both save atomically, writing a
//!   temporary file and renaming it over the old one, so the file is replaced on
//!   every write and a file-level watch goes deaf after the first one. That looks
//!   exactly like the user having stopped switching sessions.
//! - **A write is not a departure.** The same save is triggered by renames,
//!   reordering, sidebar width and this dashboard's own label writes, so every
//!   write is a *re-read and diff*, and `reread` decides what changed.
//! - **Read once before the first change.** The selection on screen when the
//!   watch starts is the one the user will leave next, and without a baseline the
//!   first switch after a start would read as a first sighting and depart
//!   nothing.
//!
//! **A watch that cannot run waits for its directory rather than giving up.** The
//! directory may not exist yet when the dashboard starts (the terminal installed
//! or first run while it is up), and it may be removed and recreated under a
//! running watch, which on Windows unwatches silently. So the thread waits for
//! the directory to appear, watches it, checks every [`DIR_CHECK`] that the
//! directory it watches is still the one at that path, and goes back to waiting
//! when it is not. Each re-arm starts with a baseline pass, which the adapter
//! treats as a first sighting of every window: what changed while the watch was
//! deaf cannot be placed in time. Whether a watch is running is published, so a
//! poll that depends on it can say it is not rather than look like an idle user.
//!
//! **A watch that went deaf is re-armed too.** On Windows, `notify` meets an
//! unexpected error from `ReadDirectoryChangesW` (an overflowed buffer, a volume
//! error) by logging it and unwatching the directory, with no error passed on and
//! the watcher still alive, so nothing in the event stream says the watch has
//! stopped. So every [`DIR_CHECK`] without an event the directory's listing is
//! compared with the one taken at the last read: a listing that moved with no
//! event to say so is a deaf watch, and it is re-armed with a baseline like a
//! replaced directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::Observation;

/// How long a burst of file events must go quiet before the directory is
/// re-read. One atomic save produces several events (the temporary file created
/// and written, the old file replaced), and they collapse here into one read.
pub const COALESCE: Duration = Duration::from_millis(120);

/// How often a missing directory is looked for, and how often a watched one is
/// checked to still be the directory at its path.
const DIR_CHECK: Duration = Duration::from_secs(10);

/// What the notify callback hands the watch thread.
enum Event {
    Changed,
    Failed(String),
}

/// Watch `dir` on a thread of its own, call `reread` with `true` for a baseline
/// each time the watch starts and with `false` after every burst of changes, and
/// forward whatever it reports to `sink`.
///
/// `reread` owns the diff state, so the watch knows nothing of any terminal's
/// file format. The thread ends when `sink`'s receiver is gone. The returned flag
/// is `true` while a watch is in place.
pub fn spawn(terminal: &'static str, dir: PathBuf, mut reread: impl FnMut(&Path, bool) -> Vec<Observation> + Send + 'static, sink: Sender<Observation>) -> Arc<AtomicBool> {
    let watching = Arc::new(AtomicBool::new(false));
    let flag = watching.clone();
    std::thread::spawn(move || {
        let mut waiting_said = false;
        loop {
            let Some(identity) = identity(&dir) else {
                if !std::mem::replace(&mut waiting_said, true) {
                    tracing::info!(terminal, dir = %dir.display(), "no selection snapshot directory yet; waiting for it");
                }
                std::thread::sleep(DIR_CHECK);
                continue;
            };
            waiting_said = false;
            match watch_once(terminal, &dir, identity, &mut reread, &sink, &flag) {
                Ended::ConsumerGone => return,
                Ended::Failed => std::thread::sleep(DIR_CHECK),
                Ended::DirectoryChanged | Ended::Deaf => {}
            }
        }
    });
    watching
}

enum Ended {
    ConsumerGone,
    /// The watch could not be put in place; tried again after a wait.
    Failed,
    /// The directory went away or was replaced.
    DirectoryChanged,
    /// The directory changed and no event said so.
    Deaf,
}

/// What the directory's listing says without opening a file: how many entries
/// it holds and the newest write time among them. `None` when it cannot be
/// listed. Opening nothing matters on Windows, where an open file cannot be
/// renamed over and the terminal's save would be lost.
fn listing(dir: &Path) -> Option<(usize, Option<SystemTime>)> {
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    let newest = entries.iter().filter_map(|e| e.metadata().ok()?.modified().ok()).max();
    Some((entries.len(), newest))
}

/// What tells one directory at a path from another: its creation time, which a
/// directory recreated at the same path does not share. `None` when there is no
/// directory there. A platform that keeps no creation time leaves only the
/// existence check.
fn identity(dir: &Path) -> Option<Option<SystemTime>> {
    let meta = std::fs::metadata(dir).ok().filter(|m| m.is_dir())?;
    Some(meta.created().ok())
}

fn watch_once(terminal: &'static str, dir: &Path, identity_at_start: Option<SystemTime>, reread: &mut impl FnMut(&Path, bool) -> Vec<Observation>, sink: &Sender<Observation>, watching: &AtomicBool) -> Ended {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Event::Failed(e.to_string()));
                return;
            }
        };
        // Create, modify and rename all mean "a window was rewritten", because
        // an atomic save produces them interchangeably; a removal is a window
        // closing, which the re-read notices by the file being gone.
        if matches!(event.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)) {
            let _ = tx.send(Event::Changed);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(terminal, error = %e, "selection watcher create failed; retrying");
            return Ended::Failed;
        }
    };
    if let Err(e) = notify::Watcher::watch(&mut watcher, dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!(terminal, dir = %dir.display(), error = %e, "selection watch failed; retrying");
        return Ended::Failed;
    }
    watching.store(true, Ordering::Relaxed);
    tracing::info!(terminal, dir = %dir.display(), "watching the selection snapshot");
    let ended = run(terminal, dir, identity_at_start, reread, sink, &rx);
    watching.store(false, Ordering::Relaxed);
    match ended {
        Ended::DirectoryChanged => tracing::warn!(terminal, dir = %dir.display(), "the selection snapshot directory went away or was replaced; the watch stops until it is back"),
        Ended::Deaf => tracing::warn!(terminal, dir = %dir.display(), "the selection snapshot directory changed with no event to say so; the watch is re-armed"),
        Ended::ConsumerGone | Ended::Failed => {}
    }
    ended
}

fn run(terminal: &'static str, dir: &Path, identity_at_start: Option<SystemTime>, reread: &mut impl FnMut(&Path, bool) -> Vec<Observation>, sink: &Sender<Observation>, rx: &Receiver<Event>) -> Ended {
    // The listing is taken before each read, so a change landing during the read
    // differs from it and is either announced by its event or caught as deafness.
    let mut seen = listing(dir);
    // The baseline, read after the watch is in place so a save landing between
    // the two is still seen.
    if !forward(reread(dir, true), sink) {
        return Ended::ConsumerGone;
    }
    loop {
        let changed = match rx.recv_timeout(DIR_CHECK) {
            Ok(Event::Changed) => true,
            Ok(Event::Failed(error)) => {
                tracing::warn!(terminal, dir = %dir.display(), error = %error, "the selection watch reported an error");
                false
            }
            // The listing moved with no event. One may still be in flight from a
            // save made just now, so it gets a coalesce to arrive.
            Err(RecvTimeoutError::Timeout) if listing(dir) != seen => match rx.recv_timeout(COALESCE) {
                Ok(Event::Changed) => true,
                Ok(Event::Failed(_)) | Err(RecvTimeoutError::Timeout) => return Ended::Deaf,
                Err(RecvTimeoutError::Disconnected) => return Ended::DirectoryChanged,
            },
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return Ended::DirectoryChanged,
        };
        if changed {
            loop {
                match rx.recv_timeout(COALESCE) {
                    Ok(Event::Changed) => {}
                    Ok(Event::Failed(error)) => tracing::warn!(terminal, dir = %dir.display(), error = %error, "the selection watch reported an error"),
                    Err(_) => break,
                }
            }
            seen = listing(dir);
            if !forward(reread(dir, false), sink) {
                return Ended::ConsumerGone;
            }
        }
        if identity(dir) != Some(identity_at_start) {
            return Ended::DirectoryChanged;
        }
    }
}

/// Send every observation, answering whether the consumer is still there.
fn forward(observations: Vec<Observation>, sink: &Sender<Observation>) -> bool {
    observations.into_iter().all(|o| sink.send(o).is_ok())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    use super::*;
    use crate::terminals::TerminalSession;

    /// A directory no other test or process uses. Canonical because macOS's
    /// temporary directory sits behind a symlink, and FSEvents reports the
    /// resolved path.
    fn scratch_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("ccdash-snapshot-watch-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// Save the way agterm and agwinterm do: a temporary file renamed over the
    /// real one.
    fn atomic_save(dir: &Path, text: &str) {
        let tmp = dir.join("w1.json.tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, dir.join("w1.json")).unwrap();
    }

    #[test]
    fn the_watch_reads_once_at_the_start_and_again_after_every_atomic_save() {
        let dir = scratch_dir();
        let (tx, rx) = mpsc::channel();
        let mut calls = 0u32;
        spawn(
            "test",
            dir.clone(),
            move |_, baseline| {
                calls += 1;
                vec![Observation { terminal: "test", session: TerminalSession { cwd: None, title: Some(format!("read {calls} {baseline}")) }, at_ms: 0, ..crate::terminals::verdict_tests::departure() }]
            },
            tx,
        );
        let next = || rx.recv_timeout(Duration::from_secs(10)).expect("a re-read").session.title.unwrap();
        assert_eq!(next(), "read 1 true", "the baseline is read before any change");
        atomic_save(&dir, "one");
        assert_eq!(next(), "read 2 false");
        // The second save replaces the file the first one created. A watch on the
        // file rather than the directory would have gone deaf here.
        atomic_save(&dir, "two");
        assert_eq!(next(), "read 3 false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_that_appears_after_the_start_is_watched_once_it_does() {
        // The terminal installed or first run while the dashboard is up.
        let dir = scratch_dir().join("windows");
        let (tx, rx) = mpsc::channel();
        let watching = spawn("test", dir.clone(), |_, baseline| vec![Observation { terminal: "test", session: TerminalSession { cwd: None, title: Some(baseline.to_string()) }, at_ms: 0, ..crate::terminals::verdict_tests::departure() }], tx);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "nothing is read while there is no directory");
        assert!(!watching.load(Ordering::Relaxed), "and the watch says it is not running");
        std::fs::create_dir_all(&dir).unwrap();
        let first = rx.recv_timeout(DIR_CHECK * 2).expect("the directory is picked up");
        assert_eq!(first.session.title.as_deref(), Some("true"), "starting with a baseline");
        assert!(watching.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_change_no_event_announced_ends_the_watch_as_deaf() {
        // The watcher stopped delivering while staying alive, as notify's
        // Windows backend does after an unexpected error: here no watcher is
        // attached at all, and the channel stays open.
        let dir = scratch_dir();
        atomic_save(&dir, "one");
        let (_events, rx) = mpsc::channel::<Event>();
        let (sink, _observations) = mpsc::channel();
        let identity_at_start = identity(&dir).unwrap();
        let watched = dir.clone();
        let (done, ended) = mpsc::channel();
        std::thread::spawn(move || done.send(run("test", &watched, identity_at_start, &mut |_, _| Vec::new(), &sink, &rx)));
        std::thread::sleep(Duration::from_millis(300));
        atomic_save(&dir, "two, with no event");
        let ended = ended.recv_timeout(DIR_CHECK * 2).expect("the watch kept waiting on a watcher that had gone deaf");
        assert!(matches!(ended, Ended::Deaf));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
