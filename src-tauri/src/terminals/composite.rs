//! Several adapters answering as one.
//!
//! On Windows two terminals can host a session at once: Windows Terminal, whose
//! adapter answers restore and the stale check, and agwinterm, whose adapter
//! answers labelling; both answer attention. [`super::for_platform`] still
//! returns one adapter, so every consumer keeps asking one question of one thing
//! and none of them learns there are two. What it costs is a stated rule per method for turning
//! several answers into one, and each rule is below beside the method it governs.
//!
//! **Children must not collide on what they mint.** A surface key goes out
//! unprefixed, because three readers already hold surface keys and compare them
//! for equality, so each child must use its own prefix (Windows Terminal mints
//! `hwnd:`, agwinterm mints none). A label key goes out as
//! `"{child name}|{child key}"`, since only [`Composite::write_label`] reads it
//! back and it needs to know whose it is. An [`Observation`] is passed on as the
//! child made it, carrying that child's slug, so the decision log says which
//! terminal saw what rather than naming the set.

use std::sync::mpsc::Sender;

use super::{FrontReading, LabelBudget, LabelTarget, LabelWrite, Observation, TerminalAdapter, TerminalSession, FALLBACK_STALE_REMEDY};

pub struct Composite {
    name: &'static str,
    children: Vec<Box<dyn TerminalAdapter>>,
}

impl Composite {
    /// `name` is the slug the decision log carries for the whole set. It is the
    /// caller's to choose so that wrapping an existing adapter keeps the slug its
    /// consumers already log under.
    pub fn new(name: &'static str, children: Vec<Box<dyn TerminalAdapter>>) -> Self {
        Self { name, children }
    }
}

/// The union of the children that answered, or `None` when none did.
///
/// The rule `sessions`, `front_readings` and `label_targets` share, because all
/// three draw the same line: `None` is "could not look" and `Some` is a finding.
/// One child that could not look does not make the others' findings unknown, so
/// it is skipped rather than allowed to veto; only a set where nobody looked is a
/// failure to look. That keeps restore's retry gate where it was: a child that
/// never answers this question contributes nothing, and the answer is whatever
/// the others say, `Some(vec![])` included.
fn combine_capability<T>(answers: impl IntoIterator<Item = Option<Vec<T>>>) -> Option<Vec<T>> {
    answers.into_iter().fold(None, |acc, answer| match (acc, answer) {
        (Some(mut all), Some(more)) => {
            all.extend(more);
            Some(all)
        }
        (acc, answer) => acc.or(answer),
    })
}

/// The child name and the child's own key inside a label key this composite
/// minted, or `None` for a key it did not mint.
fn route_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('|')
}

/// The first remedy a child wrote for itself, else the generic one.
///
/// One string has to serve every stale alert, because the flag does not record
/// which terminal raised it. That holds because only Windows Terminal implements
/// `front_readings`, so only it can raise the flag; a second child that did would
/// need the flag to carry its terminal first.
fn first_specific_remedy(remedies: impl IntoIterator<Item = &'static str>) -> &'static str {
    remedies.into_iter().find(|r| *r != FALLBACK_STALE_REMEDY).unwrap_or(FALLBACK_STALE_REMEDY)
}

impl TerminalAdapter for Composite {
    fn name(&self) -> &'static str {
        self.name
    }

    fn sessions(&self) -> Option<Vec<TerminalSession>> {
        combine_capability(self.children.iter().map(|c| c.sessions()))
    }

    /// Every child's observations, concatenated, each still carrying the slug of
    /// the child that made it. Each child keeps its own diff state, so nothing is
    /// lost or doubled by asking them in turn.
    fn poll(&mut self, now_ms: i64) -> Vec<Observation> {
        self.children.iter_mut().flat_map(|c| c.poll(now_ms)).collect()
    }

    /// Every child gets the sink, so whichever terminal sees an edge reports it.
    fn watch(&self, sink: Sender<Observation>) {
        for c in &self.children {
            c.watch(sink.clone());
        }
    }

    fn front_readings(&self) -> Option<Vec<FrontReading>> {
        combine_capability(self.children.iter().map(|c| c.front_readings()))
    }

    fn stale_remedy(&self) -> &'static str {
        first_specific_remedy(self.children.iter().map(|c| c.stale_remedy()))
    }

    /// The first child that recognizes the console. A console is rendered by at
    /// most one terminal, so a second answer could only be a mistake, and asking
    /// no further keeps the call as cheap as the trait requires.
    fn attached_surface(&self, pid: u32) -> Option<String> {
        self.children.iter().find_map(|c| c.attached_surface(pid))
    }

    /// Every child, on the calling thread, since any of them may be asked for
    /// `front_readings` from it.
    fn prepare_reader(&self) {
        for c in &self.children {
            c.prepare_reader();
        }
    }

    /// Whether any child has a context line: one that does is enough for the
    /// worker to have something to ask.
    fn can_label(&self) -> Option<bool> {
        // Any child that can label makes the composite labellable, and a child
        // that could not be asked leaves the answer open rather than settling it
        // as a no — the union rule every other read on this seam follows.
        let answers: Vec<Option<bool>> = self.children.iter().map(|c| c.can_label()).collect();
        if answers.iter().any(|a| *a == Some(true)) {
            return Some(true);
        }
        answers.iter().all(|a| a.is_some()).then_some(false)
    }

    fn label_targets(&self) -> Option<Vec<LabelTarget>> {
        combine_capability(self.children.iter().map(|c| {
            let child = c.name();
            c.label_targets().map(|ts| ts.into_iter().map(|t| LabelTarget { key: format!("{child}|{}", t.key), ..t }).collect())
        }))
    }

    fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
        let (child, inner) = route_key(key).ok_or_else(|| format!("label key {key:?} names no terminal"))?;
        let target = self.children.iter().find(|c| c.name() == child).ok_or_else(|| format!("no terminal named {child:?} is in this set"))?;
        target.write_label(inner, write)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A child whose every answer is set by the test.
    #[derive(Default)]
    struct Fake {
        name: &'static str,
        sessions: Option<Vec<TerminalSession>>,
        remedy: Option<&'static str>,
        surface: Option<String>,
        targets: Option<Vec<LabelTarget>>,
        labels: bool,
        writes: Arc<Mutex<Vec<(String, LabelWrite)>>>,
        watched: Arc<Mutex<u32>>,
    }

    impl TerminalAdapter for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn sessions(&self) -> Option<Vec<TerminalSession>> {
            self.sessions.clone()
        }
        fn poll(&mut self, now_ms: i64) -> Vec<Observation> {
            vec![Observation { terminal: self.name, session: tab(self.name), at_ms: now_ms, ..crate::terminals::verdict_tests::input() }]
        }
        fn watch(&self, sink: Sender<Observation>) {
            *self.watched.lock().unwrap() += 1;
            sink.send(Observation { terminal: self.name, session: tab(self.name), at_ms: 0, ..crate::terminals::verdict_tests::departure() }).unwrap();
        }
        fn stale_remedy(&self) -> &'static str {
            self.remedy.unwrap_or(FALLBACK_STALE_REMEDY)
        }
        fn attached_surface(&self, _pid: u32) -> Option<String> {
            self.surface.clone()
        }
        fn can_label(&self) -> Option<bool> {
            Some(self.labels)
        }
        fn label_targets(&self) -> Option<Vec<LabelTarget>> {
            self.targets.clone()
        }
        fn write_label(&self, key: &str, write: &LabelWrite) -> Result<(), String> {
            self.writes.lock().unwrap().push((key.to_string(), write.clone()));
            Ok(())
        }
    }

    fn tab(title: &str) -> TerminalSession {
        TerminalSession { cwd: None, title: Some(title.to_string()) }
    }

    fn target(key: &str) -> LabelTarget {
        LabelTarget { key: key.to_string(), title: Some("🔵 x".to_string()), context: None, budget: LabelBudget::Utf16(200) }
    }

    fn composite(children: Vec<Fake>) -> Composite {
        Composite::new("windows", children.into_iter().map(|c| Box::new(c) as Box<dyn TerminalAdapter>).collect())
    }

    #[test]
    fn combine_capability_is_none_only_when_nobody_looked() {
        assert_eq!(combine_capability::<u8>([None, None]), None);
        assert_eq!(combine_capability::<u8>([None, Some(vec![])]), Some(vec![]));
        assert_eq!(combine_capability::<u8>([Some(vec![])]), Some(vec![]));
        assert_eq!(combine_capability([Some(vec![1, 2]), None, Some(vec![3])]), Some(vec![1, 2, 3]), "the union keeps each child's order, children in turn");
    }

    #[test]
    fn restore_answer_is_unchanged_when_agwinterm_declines() {
        // agwinterm answers `None` to `sessions` because it is not a restore
        // witness. The console's answer must come through exactly as it did when
        // it was the only adapter, its retry-gating `None` included.
        let agw = || Fake { name: "agwinterm", ..Fake::default() };
        let console = |s: Option<Vec<TerminalSession>>| Fake { name: "windows", sessions: s, ..Fake::default() };
        assert_eq!(composite(vec![console(Some(vec![tab("🟢 x")])), agw()]).sessions(), Some(vec![tab("🟢 x")]));
        assert_eq!(composite(vec![console(Some(vec![])), agw()]).sessions(), Some(vec![]));
        assert_eq!(composite(vec![console(None), agw()]).sessions(), None, "a console that could not look still asks restore to retry");
    }

    #[test]
    fn keys_round_trip_through_the_child_prefix() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let c = composite(vec![
            Fake { name: "windows", ..Fake::default() },
            Fake { name: "agwinterm", targets: Some(vec![target("w1/p1")]), writes: writes.clone(), ..Fake::default() },
        ]);
        let targets = c.label_targets().unwrap();
        assert_eq!(targets.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), ["agwinterm|w1/p1"]);
        c.write_label(&targets[0].key, &LabelWrite::ClearContext).unwrap();
        assert_eq!(*writes.lock().unwrap(), vec![("w1/p1".to_string(), LabelWrite::ClearContext)], "the child gets back exactly the key it minted");
    }

    #[test]
    fn an_unknown_prefix_routes_nowhere() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let c = composite(vec![Fake { name: "agwinterm", writes: writes.clone(), ..Fake::default() }]);
        assert!(c.write_label("kitty|w1/p1", &LabelWrite::ClearContext).is_err());
        assert!(c.write_label("no separator", &LabelWrite::ClearContext).is_err());
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn the_set_labels_when_any_child_does() {
        assert_eq!(composite(vec![Fake { name: "windows", ..Fake::default() }]).can_label(), Some(false), "a terminal that never overrides it has no context line");
        assert_eq!(
            composite(vec![Fake { name: "windows", ..Fake::default() }, Fake { name: "agwinterm", labels: true, ..Fake::default() }]).can_label(),
            Some(true)
        );
    }

    #[test]
    fn first_specific_remedy_wins() {
        let c = composite(vec![Fake { name: "agwinterm", ..Fake::default() }, Fake { name: "windows", remedy: Some("reset the tab"), ..Fake::default() }]);
        assert_eq!(c.stale_remedy(), "reset the tab");
        assert_eq!(composite(vec![Fake { name: "a", ..Fake::default() }]).stale_remedy(), FALLBACK_STALE_REMEDY);
    }

    #[test]
    fn attached_surface_takes_the_first_some() {
        let c = composite(vec![
            Fake { name: "a", ..Fake::default() },
            Fake { name: "b", surface: Some("hwnd:1".to_string()), ..Fake::default() },
            Fake { name: "c", surface: Some("hwnd:2".to_string()), ..Fake::default() },
        ]);
        assert_eq!(c.attached_surface(7), Some("hwnd:1".to_string()));
    }

    #[test]
    fn watch_gives_every_child_the_sink() {
        let (a, b) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
        let c = composite(vec![Fake { name: "a", watched: a.clone(), ..Fake::default() }, Fake { name: "b", watched: b.clone(), ..Fake::default() }]);
        let (tx, rx) = mpsc::channel();
        c.watch(tx);
        assert_eq!((*a.lock().unwrap(), *b.lock().unwrap()), (1, 1));
        let terminals: Vec<_> = rx.try_iter().map(|o| o.terminal).collect();
        assert_eq!(terminals, ["a", "b"], "each observation names the child that saw it, not the set");
    }

    #[test]
    fn poll_concatenates_every_child() {
        let mut c = composite(vec![Fake { name: "a", ..Fake::default() }, Fake { name: "b", ..Fake::default() }]);
        let terminals: Vec<_> = c.poll(5).into_iter().map(|o| o.terminal).collect();
        assert_eq!(terminals, ["a", "b"], "each observation names the child that saw it, not the set");
    }
}
