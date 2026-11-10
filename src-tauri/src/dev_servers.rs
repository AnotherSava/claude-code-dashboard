//! Dev servers started from a terminal session, shown on that session until
//! they stop — and on the tab of another machine that renders it.
//!
//! A server is *announced* rather than discovered: the deploy skill's
//! `deploy-dev-server.sh` posts `POST /api/dev-server` once the port it
//! launched answers, carrying the project directory, the port and the address
//! of the terminal session it ran in (`AGTERM_SESSION_ID`). Scanning listeners
//! instead (`lsof`, then each pid's environment) was the first design and was
//! dropped for this: the script knows the moment the server is ready, which a
//! scan learns up to a whole interval later, and it names the one server worth
//! opening, where a scan sees every socket a session ever opened — a debugger, a
//! language server — and needs a probe to tell a page from the rest. The cost is
//! that a server started any other way is never shown.
//!
//! Nothing announces a stop: the server runs detached and outlives the script.
//! So a worker checks every announced port each [`PASS`] and forgets one that
//! no longer accepts a connection, clearing its line. A redeploy kills the old
//! server before starting the new one, so a pass landing in that gap forgets
//! it and the new server's announcement brings it back a few seconds later.
//!
//! Announcements are kept in `dev_servers.json` because a server outlives a
//! dashboard restart, which every deploy of the dashboard is, and nothing
//! would announce it again. The line itself expires after [`EXPIRE`] unless
//! rewritten, so a dashboard that stops leaves nothing claiming a server runs.
//!
//! **A server can be shown on another machine.** An agent on the Windows box
//! runs under the WSL tmux holder, so it has no agterm session to name, and the
//! Mac reaches it through a `⇄` tab that runs only the attach. So every
//! announcement also records the dashboard row it belongs to and the tailnet
//! address the deploy published, the servers that carry a row ride the sync
//! push ([`SharedServer`], `SyncPush::dev_servers`), and the receiving worker
//! finds each tab whose badged title names that synced row — the join
//! `attention::resolve_row` makes — and writes the same line there. The ports
//! are checked by the machine they run on, never by the one showing them, so a
//! peer's list is taken as its last push stated it. The chord opens such a
//! server at its tailnet address, since `localhost` names the wrong machine; a
//! server whose deploy published none is shown and cannot be opened.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::json_store;
use crate::state::AgentSession;
use crate::terminals::{NamedBy, ServerTarget, ServersShown, TerminalAdapter, TerminalSession};

pub const FILE_NAME: &str = "dev_servers.json";

/// How often announced ports are checked and their lines rewritten.
const PASS: Duration = Duration::from_secs(15);

/// How long a line stands without being rewritten. Three passes, so one slow
/// or failed write does not blink it off.
const EXPIRE: Duration = Duration::from_secs(45);

/// How long one port is given to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevServer {
    pub port: u16,
    pub dir: String,
    /// The terminal session the server was started from, as its environment
    /// named it. Opaque: only the adapter reads it. `None` where the announcing
    /// process ran in no terminal that gives one — a Windows agent under the
    /// WSL tmux holder — so the server is shown only on another machine's tab.
    #[serde(default)]
    pub session: Option<String>,
    /// The dashboard row the server belongs to, unprefixed, which a peer shows
    /// it under. `None` on a record kept from before rows were recorded.
    #[serde(default)]
    pub row: Option<String>,
    /// The address that opens it from another machine: the tailnet front the
    /// deploy published, `None` where it published none.
    #[serde(default)]
    pub url: Option<String>,
    pub announced_at: i64,
}

/// A dev server as it rides the sync push. `announced_at` is on the sender's
/// clock and only ever compared with the same sender's other servers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedServer {
    /// The sender's row id, not namespaced, as `SessionSync` ids arrive.
    pub row: String,
    pub port: u16,
    #[serde(default)]
    pub url: Option<String>,
    pub announced_at: i64,
}

/// One server the chord can be offered for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    pub port: u16,
    /// The machine it runs on, `None` for this one.
    pub device: Option<String>,
    /// The URL that opens it from here, `None` for a peer's server whose deploy
    /// published no tailnet address.
    pub open: Option<String>,
}

pub struct DevServers {
    path: PathBuf,
    held: Mutex<Held>,
    wake: Condvar,
}

struct Held {
    servers: Vec<DevServer>,
    /// Each peer's servers as its last push listed them, keyed by device.
    remote: HashMap<String, Vec<SharedServer>>,
    /// The peer servers the last pass showed on each tab, keyed by the tab's
    /// session address, with the device they run on. What the chord is answered
    /// from on a `⇄` tab.
    tabs: BTreeMap<String, (String, Vec<SharedServer>)>,
    /// Something to show changed since the worker last woke. Kept beside the
    /// list rather than left to the condvar, so one that arrives while a pass
    /// is running still cuts the next wait short.
    announced: bool,
}

impl DevServers {
    pub fn new(path: PathBuf) -> Self {
        let data = json_store::load(&path, "dev server list");
        Self { path, held: Mutex::new(Held { servers: data, remote: HashMap::new(), tabs: BTreeMap::new(), announced: false }), wake: Condvar::new() }
    }

    /// Record a server and wake the worker, so its line appears now rather
    /// than at the next pass.
    pub fn announce(&self, server: DevServer) {
        let mut held = self.held.lock().unwrap();
        announce(&mut held.servers, server);
        self.save(&held.servers);
        held.announced = true;
        self.wake.notify_all();
    }

    /// Replace one peer's servers with what its push lists, waking the worker
    /// only when that changed anything, since a push arrives every 30s.
    pub fn set_remote(&self, device: &str, servers: Vec<SharedServer>) {
        let mut held = self.held.lock().unwrap();
        let before = held.remote.get(device).map(Vec::as_slice).unwrap_or(&[]);
        if before == servers.as_slice() {
            return;
        }
        if servers.is_empty() {
            held.remote.remove(device);
        } else {
            held.remote.insert(device.to_string(), servers);
        }
        held.announced = true;
        self.wake.notify_all();
    }

    /// The servers this machine announced that a peer can show: those
    /// recorded with a row.
    pub fn shared(&self) -> Vec<SharedServer> {
        let held = self.held.lock().unwrap();
        held.servers.iter().filter_map(|s| Some(SharedServer { row: s.row.clone()?, port: s.port, url: s.url.clone(), announced_at: s.announced_at })).collect()
    }

    /// The port and published tailnet URL of each server this machine
    /// recorded for `row`, for [`crate::session_leftovers`] to stop and
    /// unpublish once that row's last session ends.
    pub fn servers_of_row(&self, row: &str) -> Vec<(u16, Option<String>)> {
        let held = self.held.lock().unwrap();
        held.servers.iter().filter(|s| s.row.as_deref() == Some(row)).map(|s| (s.port, s.url.clone())).collect()
    }

    /// The servers started from `session`, newest first, then the peer
    /// servers the last pass showed on it, newest first.
    pub fn for_session(&self, session: &str) -> Vec<Offered> {
        let held = self.held.lock().unwrap();
        let mut local: Vec<&DevServer> = held.servers.iter().filter(|s| s.session.as_deref() == Some(session)).collect();
        local.sort_by(|a, b| b.announced_at.cmp(&a.announced_at));
        let mut out: Vec<Offered> = local.iter().map(|s| Offered { port: s.port, device: None, open: Some(format!("http://localhost:{}", s.port)) }).collect();
        if let Some((device, remote)) = held.tabs.get(session) {
            let mut remote: Vec<&SharedServer> = remote.iter().collect();
            remote.sort_by(|a, b| b.announced_at.cmp(&a.announced_at));
            out.extend(remote.iter().map(|s| Offered { port: s.port, device: Some(device.clone()), open: s.url.clone() }));
        }
        out
    }

    fn snapshot(&self) -> Vec<DevServer> {
        self.held.lock().unwrap().servers.clone()
    }

    fn remote_snapshot(&self) -> HashMap<String, Vec<SharedServer>> {
        self.held.lock().unwrap().remote.clone()
    }

    fn set_tabs(&self, tabs: BTreeMap<String, (String, Vec<SharedServer>)>) {
        self.held.lock().unwrap().tabs = tabs;
    }

    /// Forget each `(session, port)` pair, persisting only when one was held,
    /// and say whether one was. Keyed by the pair, so a server announced on the
    /// same port from another session while the pass was checking the old one
    /// is kept. One announced again from the same session in that window, which
    /// is what a redeploy sends, is forgotten with the old one and stays unshown
    /// until the next redeploy announces it again.
    fn forget(&self, gone: &[(Option<String>, u16)]) -> bool {
        let mut held = self.held.lock().unwrap();
        let before = held.servers.len();
        held.servers.retain(|s| !gone.iter().any(|(session, port)| *session == s.session && *port == s.port));
        let changed = held.servers.len() != before;
        if changed {
            self.save(&held.servers);
        }
        changed
    }

    /// Sleep until something to show changes or `timeout`, whichever comes
    /// first.
    fn wait(&self, timeout: Duration) {
        let held = self.held.lock().unwrap();
        let mut held = self.wake.wait_timeout_while(held, timeout, |h| !h.announced).unwrap().0;
        held.announced = false;
    }

    fn save(&self, data: &[DevServer]) {
        json_store::save(&self.path, data, "dev server list");
    }
}

/// The pure half of [`DevServers::announce`]. One port is one server on this
/// machine, so an announcement replaces whatever held the port before, from
/// whichever session.
fn announce(data: &mut Vec<DevServer>, server: DevServer) {
    data.retain(|s| s.port != server.port);
    data.push(server);
}

/// Each session's ports, sorted, for the line that names them. A server
/// started from no session has no line here.
fn lines(servers: &[DevServer]) -> BTreeMap<String, Vec<u16>> {
    let mut out: BTreeMap<String, Vec<u16>> = BTreeMap::new();
    for s in servers {
        if let Some(session) = &s.session {
            out.entry(session.clone()).or_default().push(s.port);
        }
    }
    for ports in out.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }
    out
}

/// Which tabs render a peer's row that has servers, and those servers with
/// their device. A tab is joined by its badged title alone, the rule
/// `attention::resolve_row` answers with [`NamedBy::RemoteTitle`]: a title
/// naming a local row, or none, or two rows equally, shows nothing.
fn remote_tabs(targets: &[ServerTarget], rows: &[AgentSession], remote: &HashMap<String, Vec<SharedServer>>) -> BTreeMap<String, (String, Vec<SharedServer>)> {
    let mut out = BTreeMap::new();
    for t in targets {
        let tab = TerminalSession { cwd: None, title: t.title.clone() };
        let crate::attention::Resolved::Row(id, NamedBy::RemoteTitle) = crate::attention::resolve_row(&tab, rows, None) else { continue };
        let Some(device) = rows.iter().find(|r| r.id == id).and_then(|r| r.origin.clone()) else { continue };
        let Some(raw) = crate::sync::raw_id(&id, &device) else { continue };
        let found: Vec<SharedServer> = remote.get(&device).into_iter().flatten().filter(|s| s.row == raw).cloned().collect();
        if !found.is_empty() {
            out.insert(t.session.clone(), (device, found));
        }
    }
    out
}

/// Whether something accepts a connection on `port` here, on either loopback:
/// a dev server bound to `localhost` can be listening on `::1` alone.
fn port_answers(port: u16) -> bool {
    let candidates = [SocketAddr::from(([127, 0, 0, 1], port)), SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))];
    candidates.iter().any(|addr| TcpStream::connect_timeout(addr, CONNECT_TIMEOUT).is_ok())
}

/// Started on every platform: the ports are checked wherever servers are
/// announced, so a stopped one leaves the push, while lines are written only
/// where the terminal has one to write.
pub fn spawn(app: AppHandle) {
    let adapter = crate::terminals::for_platform(&app).filter(|a| a.can_show_servers());
    std::thread::spawn(move || {
        let Some(store) = app.try_state::<DevServers>() else { return };
        let mut shown: BTreeSet<String> = BTreeSet::new();
        let mut failing: HashMap<String, String> = HashMap::new();
        loop {
            // A peer showing this machine's servers learns of one stopping now
            // rather than at the heartbeat.
            if pass(&store, adapter.as_deref(), || crate::attention::all_rows(&app), &mut shown, &mut failing) {
                crate::sync::poke(&app);
            }
            store.wait(PASS);
        }
    });
}

/// Check every announced port, forget the stopped ones, and rewrite each
/// session's line — its own servers and any peer's whose row it renders —
/// clearing the lines of sessions left with none. Answers whether a server was
/// forgotten, which the push has to carry. `rows` is asked only when a peer has
/// servers to place.
fn pass(store: &DevServers, adapter: Option<&dyn TerminalAdapter>, rows: impl FnOnce() -> Vec<AgentSession>, shown: &mut BTreeSet<String>, failing: &mut HashMap<String, String>) -> bool {
    let servers = store.snapshot();
    let (alive, stopped): (Vec<DevServer>, Vec<DevServer>) = servers.into_iter().partition(|s| port_answers(s.port));
    for s in &stopped {
        tracing::info!(decision = "dev_server", outcome = "stopped", port = s.port, dir = %s.dir, session = ?s.session, "dev server stopped answering");
    }
    let mut gone: Vec<(Option<String>, u16)> = stopped.iter().map(|s| (s.session.clone(), s.port)).collect();
    if let Some(adapter) = adapter {
        let mut wanted = lines(&alive);
        let remote = store.remote_snapshot();
        // A listing that failed keeps the tabs of the last pass, so one
        // unanswered walk does not blink every peer's line off.
        let tabs = if remote.is_empty() { BTreeMap::new() } else { adapter.server_targets().map(|t| remote_tabs(&t, &rows(), &remote)).unwrap_or_else(|| store.held.lock().unwrap().tabs.clone()) };
        for (session, (_, servers)) in &tabs {
            let ports = wanted.entry(session.clone()).or_default();
            ports.extend(servers.iter().map(|s| s.port));
            ports.sort_unstable();
            ports.dedup();
        }
        // A tab is in `tabs` only when its title wore the remote badge.
        let badged: BTreeSet<String> = tabs.keys().cloned().collect();
        store.set_tabs(tabs);
        let cleared: Vec<String> = shown.iter().filter(|s| !wanted.contains_key(*s)).cloned().collect();
        for session in cleared {
            // A clear that fails leaves the line to expire by itself.
            let _ = adapter.show_servers(&session, &[], false, EXPIRE);
            shown.remove(&session);
            failing.remove(&session);
        }
        for (session, ports) in &wanted {
            match adapter.show_servers(session, ports, badged.contains(session), EXPIRE) {
                Ok(ServersShown::Shown) => {
                    shown.insert(session.clone());
                    failing.remove(session);
                }
                Ok(ServersShown::NoSuchSession) => {
                    tracing::info!(decision = "dev_server", outcome = "session_gone", session = %session, ?ports, "the session a dev server was shown on has closed; no longer shown");
                    // Only this machine's own records: a peer's server outlives
                    // the tab here that rendered it.
                    gone.extend(alive.iter().filter(|s| s.session.as_deref() == Some(session.as_str())).map(|s| (s.session.clone(), s.port)));
                    shown.remove(session);
                    failing.remove(session);
                }
                Err(e) => {
                    // Logged when the reason changes, not every pass.
                    if failing.get(session) != Some(&e) {
                        tracing::warn!(decision = "dev_server", outcome = "show_failed", session = %session, ?ports, error = %e, "could not show dev servers on their session");
                        failing.insert(session.clone(), e);
                    }
                }
            }
        }
    }
    !gone.is_empty() && store.forget(&gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(port: u16, session: &str, at: i64) -> DevServer {
        DevServer { port, dir: "/p".into(), session: Some(session.into()), row: Some("p".into()), url: None, announced_at: at }
    }

    #[test]
    fn an_announcement_replaces_whatever_held_its_port() {
        let mut data = vec![server(5173, "A", 1), server(3000, "A", 2)];
        announce(&mut data, server(5173, "B", 3));
        assert_eq!(data, vec![server(3000, "A", 2), server(5173, "B", 3)]);
    }

    #[test]
    fn each_session_gets_its_ports_sorted() {
        let got = lines(&[server(5173, "A", 1), server(3000, "A", 2), server(8080, "B", 3)]);
        assert_eq!(got.get("A"), Some(&vec![3000, 5173]));
        assert_eq!(got.get("B"), Some(&vec![8080]));
    }

    #[test]
    fn a_port_nothing_listens_on_does_not_answer_and_a_bound_one_does() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_answers(port));
        drop(listener);
        assert!(!port_answers(port));
    }

    /// An adapter that records every write and answers from a script.
    struct Recorder {
        writes: Mutex<Vec<(String, Vec<u16>)>>,
        badged: Mutex<Vec<String>>,
        gone: Vec<String>,
        targets: Vec<ServerTarget>,
    }

    impl TerminalAdapter for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn sessions(&self) -> Option<Vec<crate::terminals::TerminalSession>> {
            None
        }
        fn poll(&mut self, _now_ms: i64) -> Vec<crate::terminals::Observation> {
            Vec::new()
        }
        fn show_servers(&self, session: &str, ports: &[u16], badged: bool, _expire_after: Duration) -> Result<ServersShown, String> {
            self.writes.lock().unwrap().push((session.to_string(), ports.to_vec()));
            if badged {
                self.badged.lock().unwrap().push(session.to_string());
            }
            Ok(if self.gone.iter().any(|g| g == session) { ServersShown::NoSuchSession } else { ServersShown::Shown })
        }
        fn server_targets(&self) -> Option<Vec<ServerTarget>> {
            Some(self.targets.clone())
        }
    }

    fn recorder(gone: Vec<String>, targets: Vec<ServerTarget>) -> Recorder {
        Recorder { writes: Mutex::new(Vec::new()), badged: Mutex::new(Vec::new()), gone, targets }
    }

    fn store_with(servers: Vec<DevServer>) -> (DevServers, PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("dev_servers_test_{}_{}.json", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let store = DevServers::new(path.clone());
        store.held.lock().unwrap().servers = servers;
        (store, path)
    }

    #[test]
    fn a_stopped_server_is_forgotten_and_its_session_cleared() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap().port();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (store, path) = store_with(vec![server(live, "A", 1), server(dead, "B", 2)]);
        let adapter = recorder(Vec::new(), Vec::new());
        let mut shown: BTreeSet<String> = ["B".to_string()].into();
        pass(&store, Some(&adapter), Vec::new, &mut shown, &mut HashMap::new());
        let writes = adapter.writes.lock().unwrap().clone();
        assert!(writes.contains(&("A".to_string(), vec![live])));
        assert!(writes.contains(&("B".to_string(), vec![])), "the session whose only server stopped is cleared");
        assert_eq!(shown, ["A".to_string()].into());
        assert_eq!(store.snapshot(), vec![server(live, "A", 1)]);
        let _ = std::fs::remove_file(path);
    }

    /// A synced row as `AppState::remote` holds it: namespaced and stamped.
    fn synced(device: &str, raw: &str) -> AgentSession {
        crate::state::test_synced_row(device, raw, 0)
    }

    fn shared(row: &str, port: u16, url: Option<&str>, at: i64) -> SharedServer {
        SharedServer { row: row.into(), port, url: url.map(str::to_string), announced_at: at }
    }

    fn target(session: &str, title: &str) -> ServerTarget {
        ServerTarget { session: session.into(), title: Some(title.into()) }
    }

    #[test]
    fn a_tab_rendering_a_peers_row_gets_that_rows_servers() {
        let rows = vec![synced("CHROME", "scheduler"), synced("CHROME", "web")];
        let remote: HashMap<String, Vec<SharedServer>> = [("CHROME".to_string(), vec![shared("scheduler", 5173, Some("https://x.ts.net:8443"), 1), shared("web", 3000, None, 2)])].into();
        let targets = vec![target("T1", "⇄ 🟢 scheduler"), target("T2", "🟢 scheduler"), target("T3", "⇄ 🔵 other")];
        let got = remote_tabs(&targets, &rows, &remote);
        assert_eq!(got.len(), 1, "only the badged title naming a synced row with servers");
        assert_eq!(got.get("T1"), Some(&("CHROME".to_string(), vec![shared("scheduler", 5173, Some("https://x.ts.net:8443"), 1)])));
    }

    #[test]
    fn a_peers_servers_are_shown_and_offered_on_its_tab_and_survive_the_tab_closing() {
        let (store, path) = store_with(Vec::new());
        store.set_remote("CHROME", vec![shared("scheduler", 5173, Some("https://x.ts.net:8443"), 1), shared("scheduler", 3000, None, 2)]);
        let adapter = recorder(Vec::new(), vec![target("T1", "⇄ ✋ scheduler")]);
        let mut shown = BTreeSet::new();
        assert!(!pass(&store, Some(&adapter), || vec![synced("CHROME", "scheduler")], &mut shown, &mut HashMap::new()));
        assert!(adapter.writes.lock().unwrap().contains(&("T1".to_string(), vec![3000, 5173])));
        assert_eq!(*adapter.badged.lock().unwrap(), vec!["T1".to_string()], "a tab rendering a peer's row is written as badged");
        let offered = store.for_session("T1");
        assert_eq!(offered.iter().map(|o| (o.port, o.open.clone())).collect::<Vec<_>>(), vec![(3000, None), (5173, Some("https://x.ts.net:8443".to_string()))], "newest first, opened at the published address only");
        assert!(offered.iter().all(|o| o.device.as_deref() == Some("CHROME")));
        let closed = recorder(vec!["T1".to_string()], vec![target("T1", "⇄ ✋ scheduler")]);
        pass(&store, Some(&closed), || vec![synced("CHROME", "scheduler")], &mut shown, &mut HashMap::new());
        assert_eq!(store.remote_snapshot().get("CHROME").map(Vec::len), Some(2), "the peer's servers are the peer's to forget");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_server_started_from_no_session_is_checked_and_shared_but_given_no_line() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap().port();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut sessionless = server(live, "A", 1);
        sessionless.session = None;
        let mut stopped = server(dead, "A", 2);
        stopped.session = None;
        let (store, path) = store_with(vec![sessionless, stopped]);
        assert!(pass(&store, None, Vec::new, &mut BTreeSet::new(), &mut HashMap::new()), "a forget is reported, so the push goes out");
        assert_eq!(store.shared(), vec![shared("p", live, None, 1)]);
        let adapter = recorder(Vec::new(), Vec::new());
        pass(&store, Some(&adapter), Vec::new, &mut BTreeSet::new(), &mut HashMap::new());
        assert!(adapter.writes.lock().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_record_from_before_rows_loads_and_is_not_shared() {
        let old: Vec<DevServer> = serde_json::from_str(r#"[{"port":5173,"dir":"/p","session":"A","announced_at":1}]"#).unwrap();
        assert_eq!(old[0].session.as_deref(), Some("A"));
        assert_eq!(old[0].row, None);
    }

    #[test]
    fn a_server_whose_session_closed_is_forgotten() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap().port();
        let (store, path) = store_with(vec![server(live, "A", 1)]);
        let adapter = recorder(vec!["A".to_string()], Vec::new());
        let mut shown = BTreeSet::new();
        pass(&store, Some(&adapter), Vec::new, &mut shown, &mut HashMap::new());
        assert!(store.snapshot().is_empty());
        assert!(shown.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
