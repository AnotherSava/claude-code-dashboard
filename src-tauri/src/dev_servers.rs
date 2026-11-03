//! Local dev servers started from a terminal session, shown on that session
//! until they stop.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::json_store;
use crate::terminals::{ServersShown, TerminalAdapter};

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
    /// named it. Opaque: only the adapter reads it.
    pub session: String,
    pub announced_at: i64,
}

pub struct DevServers {
    path: PathBuf,
    held: Mutex<Held>,
    wake: Condvar,
}

struct Held {
    servers: Vec<DevServer>,
    /// An announcement arrived since the worker last woke. Kept beside the
    /// list rather than left to the condvar, so one that arrives while a pass
    /// is running still cuts the next wait short.
    announced: bool,
}

impl DevServers {
    pub fn new(path: PathBuf) -> Self {
        let data = json_store::load(&path, "dev server list");
        Self { path, held: Mutex::new(Held { servers: data, announced: false }), wake: Condvar::new() }
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

    /// Every announced server started from `session`, newest first.
    pub fn for_session(&self, session: &str) -> Vec<DevServer> {
        let mut found: Vec<DevServer> = self.held.lock().unwrap().servers.iter().filter(|s| s.session == session).cloned().collect();
        found.sort_by(|a, b| b.announced_at.cmp(&a.announced_at));
        found
    }

    fn snapshot(&self) -> Vec<DevServer> {
        self.held.lock().unwrap().servers.clone()
    }

    /// Forget each `(session, port)` pair, persisting only when one was held.
    /// Keyed by the pair, so a server announced on the same port from another
    /// session while the pass was checking the old one is kept. One announced
    /// again from the same session in that window, which is what a redeploy
    /// sends, is forgotten with the old one and stays unshown until the next
    /// redeploy announces it again.
    fn forget(&self, gone: &[(String, u16)]) {
        let mut held = self.held.lock().unwrap();
        let before = held.servers.len();
        held.servers.retain(|s| !gone.iter().any(|(session, port)| *session == s.session && *port == s.port));
        if held.servers.len() != before {
            self.save(&held.servers);
        }
    }

    /// Sleep until an announcement or `timeout`, whichever comes first.
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

/// Each session's ports, sorted, for the line that names them.
fn lines(servers: &[DevServer]) -> BTreeMap<String, Vec<u16>> {
    let mut out: BTreeMap<String, Vec<u16>> = BTreeMap::new();
    for s in servers {
        out.entry(s.session.clone()).or_default().push(s.port);
    }
    for ports in out.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }
    out
}

/// Whether something accepts a connection on `port` here, on either loopback:
/// a dev server bound to `localhost` can be listening on `::1` alone.
fn port_answers(port: u16) -> bool {
    let candidates = [SocketAddr::from(([127, 0, 0, 1], port)), SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))];
    candidates.iter().any(|addr| TcpStream::connect_timeout(addr, CONNECT_TIMEOUT).is_ok())
}

pub fn spawn(app: AppHandle) {
    let Some(adapter) = crate::terminals::for_platform(&app).filter(|a| a.can_show_servers()) else { return };
    std::thread::spawn(move || {
        let Some(store) = app.try_state::<DevServers>() else { return };
        let mut shown: BTreeSet<String> = BTreeSet::new();
        let mut failing: HashMap<String, String> = HashMap::new();
        loop {
            pass(&store, adapter.as_ref(), &mut shown, &mut failing);
            store.wait(PASS);
        }
    });
}

/// Check every announced port, forget the stopped ones, and rewrite each
/// session's line, clearing the lines of sessions left with none.
fn pass(store: &DevServers, adapter: &dyn TerminalAdapter, shown: &mut BTreeSet<String>, failing: &mut HashMap<String, String>) {
    let servers = store.snapshot();
    let (alive, stopped): (Vec<DevServer>, Vec<DevServer>) = servers.into_iter().partition(|s| port_answers(s.port));
    for s in &stopped {
        tracing::info!(decision = "dev_server", outcome = "stopped", port = s.port, dir = %s.dir, session = %s.session, "dev server stopped answering");
    }
    let mut gone: Vec<(String, u16)> = stopped.iter().map(|s| (s.session.clone(), s.port)).collect();
    let wanted = lines(&alive);
    let cleared: Vec<String> = shown.iter().filter(|s| !wanted.contains_key(*s)).cloned().collect();
    for session in cleared {
        // A clear that fails leaves the line to expire by itself.
        let _ = adapter.show_servers(&session, &[], EXPIRE);
        shown.remove(&session);
        failing.remove(&session);
    }
    for (session, ports) in &wanted {
        match adapter.show_servers(session, ports, EXPIRE) {
            Ok(ServersShown::Shown) => {
                shown.insert(session.clone());
                failing.remove(session);
            }
            Ok(ServersShown::NoSuchSession) => {
                tracing::info!(decision = "dev_server", outcome = "session_gone", session = %session, ?ports, "the session a dev server was started from has closed; no longer shown");
                gone.extend(ports.iter().map(|p| (session.clone(), *p)));
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
    if !gone.is_empty() {
        store.forget(&gone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(port: u16, session: &str, at: i64) -> DevServer {
        DevServer { port, dir: "/p".into(), session: session.into(), announced_at: at }
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
        gone: Vec<String>,
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
        fn show_servers(&self, session: &str, ports: &[u16], _expire_after: Duration) -> Result<ServersShown, String> {
            self.writes.lock().unwrap().push((session.to_string(), ports.to_vec()));
            Ok(if self.gone.iter().any(|g| g == session) { ServersShown::NoSuchSession } else { ServersShown::Shown })
        }
    }

    fn store_with(servers: Vec<DevServer>) -> (DevServers, PathBuf) {
        let path = std::env::temp_dir().join(format!("dev_servers_test_{}_{}.json", std::process::id(), servers.first().map_or(0, |s| s.port)));
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
        let adapter = Recorder { writes: Mutex::new(Vec::new()), gone: Vec::new() };
        let mut shown: BTreeSet<String> = ["B".to_string()].into();
        pass(&store, &adapter, &mut shown, &mut HashMap::new());
        let writes = adapter.writes.lock().unwrap().clone();
        assert!(writes.contains(&("A".to_string(), vec![live])));
        assert!(writes.contains(&("B".to_string(), vec![])), "the session whose only server stopped is cleared");
        assert_eq!(shown, ["A".to_string()].into());
        assert_eq!(store.snapshot(), vec![server(live, "A", 1)]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_server_whose_session_closed_is_forgotten() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap().port();
        let (store, path) = store_with(vec![server(live, "A", 1)]);
        let adapter = Recorder { writes: Mutex::new(Vec::new()), gone: vec!["A".to_string()] };
        let mut shown = BTreeSet::new();
        pass(&store, &adapter, &mut shown, &mut HashMap::new());
        assert!(store.snapshot().is_empty());
        assert!(shown.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
