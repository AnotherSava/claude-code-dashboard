//! The bookkeeping every adapter does over a terminal's per-window state files,
//! once, so the two that read such files cannot drift apart in it.
//!
//! agterm on macOS and agwinterm on Windows both save each window to a file of
//! its own in one directory, and an adapter watching them diffs each window's
//! selection against what it held for that window last time. What is the
//! adapter's is the file format and what a change means. What is here is
//! everything around that, the parts that are the same for any such terminal:
//!
//! - **A file whose write time has not moved is not re-read.** Every write of any
//!   window triggers a pass over the whole directory, and stepping the windows
//!   that did not change would only repeat their last answer. A reader given the
//!   write times already held ([`read_changed`]) does not read their text
//!   either.
//! - **A read that failed keeps the window's tracking**, because a failed read
//!   says nothing about the selection.
//! - **A file in a shape the adapter does not know stands its window down**: the
//!   tracking is forgotten, so the next good read is a first sighting rather than
//!   a departure stamped at an instant nobody observed, and it is said once at
//!   warn, then once more at info when the window reads again.
//! - **A window whose file is gone was closed**, and a file that later appears
//!   under its id is a new window.
//! - **A baseline pass starts from nothing**: everything tracked from before is
//!   forgotten, because a watch that was not running cannot place in time what
//!   changed while it was not.
//!
//! Files are stepped oldest write first, so an adapter that learns something from
//! one write (agwinterm learns when its writer was held up) knows it before it
//! steps the writes that came after.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// One window file as read in a pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRead {
    /// The file's stem, which both terminals make the window's id.
    pub window: String,
    pub contents: Contents,
}

/// What a pass learned of one window file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contents {
    /// The text and when it was written, in Unix ms.
    Read(String, i64),
    /// Its write time is the one the caller already holds, so its text was not
    /// read. The window is still there.
    Unchanged,
    /// Why it could not be read.
    Failed(String),
}

/// Read every `*.json` in `dir`, or `None` when the directory cannot be listed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn read_dir(dir: &Path) -> Option<Vec<FileRead>> {
    read_changed(dir, &HashMap::new())
}

/// Read every `*.json` in `dir` whose write time is not the one `known` holds
/// for its window, or `None` when the directory cannot be listed.
///
/// The text and the write time come from **one handle**, so they describe the
/// same file: both terminals save by renaming a new file over the old one, and
/// two opens of the path could pair one save's text with the next one's time.
/// The handle is held only for the read, and a file whose write time has not
/// moved is closed right after its metadata is read, since a pass runs on every
/// save of any window and its text would only repeat what was read last time.
/// While a handle is open a rename over the file fails on Windows even though
/// Rust opens it sharing delete access, so the terminal's save can collide with
/// this read; see the agwinterm module for what that costs there.
pub fn read_changed(dir: &Path, known: &HashMap<String, i64>) -> Option<Vec<FileRead>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut out = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(window) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        let contents = read_one(&path, known.get(&window).copied()).unwrap_or_else(|e| Contents::Failed(e.to_string()));
        out.push(FileRead { window, contents });
    }
    Some(out)
}

fn read_one(path: &Path, known: Option<i64>) -> std::io::Result<Contents> {
    let mut file = std::fs::File::open(path)?;
    let written = file.metadata()?.modified()?.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    if known == Some(written) {
        return Ok(Contents::Unchanged);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Contents::Read(text, written))
}

/// What an adapter's step answers for one changed file.
pub enum Stepped<T, R> {
    /// The file was read: the window's new tracking (`None` forgets it) and
    /// whatever the adapter wants back for this file.
    Read(Option<T>, R),
    /// The file is not a shape the adapter knows, and why, for the log.
    Unknown(&'static str),
}

/// Each window's tracking, and what the bookkeeping needs to remember beside it.
pub struct WindowFiles<T> {
    terminal: &'static str,
    tracked: HashMap<String, T>,
    /// Windows whose file was last read in a shape the adapter does not know.
    stood_down: HashSet<String>,
    /// The write time of each window's file as last read.
    written: HashMap<String, i64>,
}

impl<T> WindowFiles<T> {
    /// `terminal` is the slug the log lines carry.
    pub fn new(terminal: &'static str) -> Self {
        Self { terminal, tracked: HashMap::new(), stood_down: HashSet::new(), written: HashMap::new() }
    }

    /// Read by agwinterm's poll, to know which session is on screen, and by both
    /// adapters' activation checks.
    pub fn tracked(&self) -> &HashMap<String, T> {
        &self.tracked
    }

    /// One window's tracking, to amend with what the adapter learned of it
    /// outside the files, which is what both adapters' activation checks do.
    pub fn tracked_mut(&mut self, window: &str) -> Option<&mut T> {
        self.tracked.get_mut(window)
    }

    /// Each window's write time as last read, for [`read_changed`].
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn written(&self) -> &HashMap<String, i64> {
        &self.written
    }

    /// Forget a window's tracking, so its next changed file is a first sighting.
    /// For an adapter that has learned the file is behind the screen.
    pub fn forget(&mut self, window: &str) {
        self.tracked.remove(window);
    }

    /// One pass over a directory's files. `step` is called for each file whose
    /// write time moved since it was last read, oldest write first, with the
    /// window id, the window's tracking, the text and the write time; what it
    /// returns for each comes back in that order.
    pub fn pass<R>(&mut self, mut files: Vec<FileRead>, baseline: bool, mut step: impl FnMut(&str, Option<&T>, &str, i64) -> Stepped<T, R>) -> Vec<R> {
        if baseline {
            self.tracked.clear();
            self.written.clear();
        }
        let terminal = self.terminal;
        let present: HashSet<String> = files.iter().map(|f| f.window.clone()).collect();
        files.sort_by_key(|f| match f.contents {
            Contents::Read(_, w) => w,
            _ => i64::MIN,
        });
        let mut out = Vec::new();
        for FileRead { window, contents } in files {
            let (text, written_ms) = match contents {
                Contents::Read(text, written_ms) => (text, written_ms),
                Contents::Unchanged => continue,
                Contents::Failed(error) => {
                    tracing::debug!(decision = "attention_poll", terminal, source = "watch", outcome = "unreadable_file", window = %window, error = %error, "a window file could not be read this pass");
                    continue;
                }
            };
            if self.written.insert(window.clone(), written_ms) == Some(written_ms) {
                continue;
            }
            match step(&window, self.tracked.get(&window), &text, written_ms) {
                Stepped::Unknown(reason) => {
                    self.tracked.remove(&window);
                    if self.stood_down.insert(window.clone()) {
                        tracing::warn!(decision = "attention_poll", terminal, source = "watch", outcome = "stood_down", window = %window, reason, "a window file is not a shape this adapter knows; no departures from it until it is");
                    }
                }
                Stepped::Read(next, r) => {
                    if self.stood_down.remove(&window) {
                        tracing::info!(decision = "attention_poll", terminal, source = "watch", outcome = "resumed", window = %window, "a window file is readable again; watching it from its current selection");
                    }
                    match next {
                        Some(t) => self.tracked.insert(window, t),
                        None => self.tracked.remove(&window),
                    };
                    out.push(r);
                }
            }
        }
        self.tracked.retain(|w, _| present.contains(w));
        self.stood_down.retain(|w| present.contains(w));
        self.written.retain(|w, _| present.contains(w));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step that tracks the file's text as the selection and reports the
    /// change, refusing `"?"` as an unknown shape.
    fn diff(_: &str, prev: Option<&String>, text: &str, _: i64) -> Stepped<String, (Option<String>, String)> {
        match text {
            "?" => Stepped::Unknown("unknown_shape"),
            _ => Stepped::Read(Some(text.to_string()), (prev.cloned(), text.to_string())),
        }
    }

    fn file(window: &str, text: &str, written: i64) -> FileRead {
        FileRead { window: window.into(), contents: Contents::Read(text.into(), written) }
    }

    fn failed(window: &str) -> FileRead {
        FileRead { window: window.into(), contents: Contents::Failed("in use".into()) }
    }

    #[test]
    fn a_file_whose_write_time_has_not_moved_is_not_stepped_again() {
        let mut w = WindowFiles::new("test");
        assert_eq!(w.pass(vec![file("a", "s1", 10), file("b", "s9", 10)], true, diff).len(), 2, "a baseline steps every file");
        // Another window's save triggers the pass; `b` did not change.
        let out = w.pass(vec![file("a", "s2", 20), file("b", "s9", 10)], false, diff);
        assert_eq!(out, vec![(Some("s1".to_string()), "s2".to_string())]);
    }

    #[test]
    fn a_failed_read_keeps_the_tracking_and_is_read_again_next_pass() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10)], true, diff);
        assert!(w.pass(vec![failed("a")], false, diff).is_empty());
        assert_eq!(w.tracked().get("a").map(String::as_str), Some("s1"));
        // The failed read recorded no write time, so the next good read is
        // stepped against the tracking from before it.
        let out = w.pass(vec![file("a", "s2", 20)], false, diff);
        assert_eq!(out, vec![(Some("s1".to_string()), "s2".to_string())]);
    }

    #[test]
    fn an_unknown_shape_forgets_the_window_so_its_recovery_is_a_first_sighting() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10)], true, diff);
        assert!(w.pass(vec![file("a", "?", 20)], false, diff).is_empty());
        assert!(!w.tracked().contains_key("a"));
        // Good, then unknown, then good with another selection: the step sees no
        // previous selection, so nothing departs from the one before the gap.
        assert_eq!(w.pass(vec![file("a", "s2", 30)], false, diff), vec![(None, "s2".to_string())]);
    }

    #[test]
    fn a_window_whose_file_is_gone_is_forgotten() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10), file("b", "s5", 10)], true, diff);
        w.pass(vec![file("b", "s5", 10)], false, diff);
        assert!(!w.tracked().contains_key("a"));
        // A file reappearing under the closed window's id is a new window.
        assert_eq!(w.pass(vec![file("a", "s2", 10), file("b", "s5", 10)], false, diff), vec![(None, "s2".to_string())]);
    }

    #[test]
    fn a_baseline_starts_from_nothing() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10)], true, diff);
        // The watch went deaf and came back: whatever changed in between cannot be
        // placed in time, so the re-read is a first sighting.
        assert_eq!(w.pass(vec![file("a", "s2", 50)], true, diff), vec![(None, "s2".to_string())]);
    }

    #[test]
    fn files_are_stepped_oldest_write_first() {
        let mut w = WindowFiles::new("test");
        let mut order = Vec::new();
        w.pass(vec![file("late", "x", 30), failed("broken"), file("early", "y", 10)], true, |window, _, text, _| {
            order.push(window.to_string());
            Stepped::Read(Some(text.to_string()), ())
        });
        assert_eq!(order, ["early", "late"]);
    }

    #[test]
    fn an_unchanged_file_keeps_its_window_without_being_stepped() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10), file("b", "s5", 10)], true, diff);
        let unchanged = FileRead { window: "b".into(), contents: Contents::Unchanged };
        assert_eq!(w.pass(vec![file("a", "s2", 20), unchanged], false, diff), vec![(Some("s1".to_string()), "s2".to_string())]);
        assert!(w.tracked().contains_key("b"), "a file not re-read is not a closed window");
    }

    #[test]
    fn a_forgotten_window_is_seen_afresh_on_its_next_write() {
        let mut w = WindowFiles::new("test");
        w.pass(vec![file("a", "s1", 10)], true, diff);
        w.forget("a");
        assert_eq!(w.pass(vec![file("a", "s2", 20)], false, diff), vec![(None, "s2".to_string())]);
    }

    #[test]
    fn the_reader_takes_text_and_write_time_from_the_json_files_only() {
        let dir = std::env::temp_dir().join(format!("ccdash-window-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w1.json"), "{}").unwrap();
        std::fs::write(dir.join("w1.json.tmp"), "partial").unwrap();
        let files = read_dir(&dir).unwrap();
        assert_eq!(files.len(), 1);
        let Contents::Read(text, written) = files[0].contents.clone() else { panic!("{:?}", files[0]) };
        assert_eq!((files[0].window.as_str(), text.as_str()), ("w1", "{}"));
        assert!(written > 0);
        // Given that write time, the file's text is not read again; given
        // another, it is.
        assert_eq!(read_changed(&dir, &HashMap::from([("w1".to_string(), written)])).unwrap()[0].contents, Contents::Unchanged);
        assert_eq!(read_changed(&dir, &HashMap::from([("w1".to_string(), written - 1)])).unwrap()[0].contents, Contents::Read("{}".into(), written));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_dir(&dir), None, "a directory that cannot be listed is not an empty one");
    }
}
