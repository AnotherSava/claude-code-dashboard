//! Stopping what a session leaves running once it has ended.
//!
//! Ending the agent, and on agterm everything on the session's tty with it,
//! leaves two kinds of process behind. A dev server the deploy skill started is
//! detached at birth (`nohup` in a double-forked subshell on macOS,
//! `Start-Process` on Windows), so nothing ties it to the session, and
//! [`crate::dev_servers`] knows it only by the port it announced. Anything else
//! the agent started that does not die with it is its descendant only while
//! the agent runs, and orphaned once it exits, so [`Tracked`] keeps each live
//! agent's descendants as the liveness reaper's last process table showed them.
//!
//! [`on_session_end`] runs where a row's last session leaves
//! ([`crate::commands::apply_departure`]), the one place every ordinary end
//! arrives: a `SessionEnd`, and the reaper finding the process gone after an
//! `exit`, Ctrl-D, a closed tab or terminal, the idle close, or an `/exit`
//! typed through `POST /api/session/command`. A `/clear` never gets there,
//! since its session goes on in place. Gated on
//! `config.stop_session_processes`.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::liveness::Proc;

const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Each live agent's descendants, keyed by the agent's pid, with the row it
/// belongs to, as the newest process table that still listed the agent showed
/// them. An agent missing from a table keeps its last entry, since once it has
/// exited its orphans can no longer be found by their parent.
#[derive(Default)]
pub struct Tracked {
    by_pid: Mutex<HashMap<u32, (String, Vec<Proc>)>>,
}

impl Tracked {
    /// Record the descendants of every member in `members` that `table` still
    /// lists, and forget a pid no longer a member of any row.
    pub fn record(&self, table: &[Proc], members: &HashSet<(String, u32)>) {
        let listed: HashSet<u32> = table.iter().map(|p| p.pid).collect();
        let mut by_pid = self.by_pid.lock().unwrap();
        by_pid.retain(|pid, (row, _)| members.contains(&(row.clone(), *pid)));
        for (row, pid) in members {
            if listed.contains(pid) {
                by_pid.insert(*pid, (row.clone(), descendants(table, &[*pid])));
            }
        }
    }

    /// Take every process recorded for `row`'s agents.
    fn take(&self, row: &str) -> Vec<Proc> {
        let mut by_pid = self.by_pid.lock().unwrap();
        let pids: Vec<u32> = by_pid.iter().filter(|(_, (r, _))| r == row).map(|(pid, _)| *pid).collect();
        pids.into_iter().filter_map(|pid| by_pid.remove(&pid)).flat_map(|(_, procs)| procs).collect()
    }
}

/// Stop what `row`'s ended session left running, on a thread of its own so the
/// row lock the departure holds is not held across the kills.
pub fn on_session_end(app: &AppHandle, row: &str) {
    if app.try_state::<crate::config::ConfigState>().is_some_and(|c| !c.snapshot().stop_session_processes) {
        return;
    }
    let procs = app.try_state::<Tracked>().map(|t| t.take(row)).unwrap_or_default();
    let ports = app.try_state::<crate::dev_servers::DevServers>().map(|d| d.ports_of_row(row)).unwrap_or_default();
    let leftovers = Leftovers { row: row.to_string(), procs, ports };
    let app = app.clone();
    std::thread::spawn(move || {
        let stopped_servers = !leftovers.ports.is_empty();
        leftovers.stop();
        // The dev-server worker forgets each stopped server and removes its
        // tailnet front. Woken now, it catches a server that has already shut
        // down; one still closing after SIGTERM answers this check and is
        // forgotten at the next pass.
        if stopped_servers {
            if let Some(store) = app.try_state::<crate::dev_servers::DevServers>() {
                store.recheck();
            }
        }
    });
}

/// What one ended session had running beside its agent.
struct Leftovers {
    row: String,
    /// The agent's descendants as last recorded.
    procs: Vec<Proc>,
    /// The ports of the dev servers this machine recorded for the row.
    ports: Vec<u16>,
}

impl Leftovers {
    /// Stop what is still running: each process recorded that still runs
    /// under the same image, and whatever listens on each dev server's port,
    /// with its descendants. Each server's tailnet front is removed by the
    /// [`crate::dev_servers`] worker once it finds the port refusing.
    fn stop(self) {
        if self.procs.is_empty() && self.ports.is_empty() {
            return;
        }
        let Some(table) = crate::liveness::process_table() else {
            tracing::warn!(chat_id = %self.row, decision = "session_leftovers", outcome = "unreadable", "the process table could not be read, so nothing the session left was stopped");
            return;
        };
        let spared = own_lineage(&table, std::process::id());
        let mut listeners: Vec<u32> = self.ports.iter().flat_map(|port| listening_pids(*port)).filter(|pid| !spared.contains(pid)).collect();
        listeners.sort_unstable();
        listeners.dedup();
        let mut singles: HashSet<u32> = survivors(&self.procs, &table);
        singles.extend(descendants(&table, &listeners).iter().map(|p| p.pid));
        singles.retain(|pid| !spared.contains(pid) && !listeners.contains(pid));
        if singles.is_empty() && listeners.is_empty() {
            tracing::info!(chat_id = %self.row, decision = "session_leftovers", outcome = "none_left", ports = ?self.ports, "nothing the session started was still running");
            return;
        }
        let mut pids: Vec<u32> = singles.into_iter().collect();
        pids.sort_unstable();
        // A listener's tree may hold workers started after it; a recorded
        // process is killed alone, since its tree was listed and a tree kill
        // could reach this dashboard if a deploy under the agent started it.
        let mut refused: Vec<(u32, String)> = listeners.iter().filter_map(|&pid| kill(pid, true).err().map(|e| (pid, e))).collect();
        refused.extend(pids.iter().filter_map(|&pid| kill(pid, false).err().map(|e| (pid, e))));
        // A process that exited between the table read and its kill refuses
        // the kill yet is stopped, so only one still running counts.
        let after = if refused.is_empty() { None } else { crate::liveness::process_table() };
        let failed: Vec<String> = still_running(refused, &table, after.as_deref()).into_iter().map(|(pid, e)| format!("{pid}: {e}")).collect();
        tracing::info!(chat_id = %self.row, decision = "session_leftovers", outcome = if failed.is_empty() { "stopped" } else { "stop_failed" }, ports = ?self.ports, ?listeners, ?pids, ?failed, "stopped what the ended session left running");
    }
}

/// Every process below `roots` in `table`, the roots themselves excluded.
fn descendants(table: &[Proc], roots: &[u32]) -> Vec<Proc> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in table {
        // A process listed as its own parent (pid 0 on Windows) would loop.
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    let mut seen: HashSet<u32> = roots.iter().copied().collect();
    let mut queue: Vec<u32> = roots.to_vec();
    let mut out = Vec::new();
    while let Some(pid) = queue.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child.pid) {
                out.push((*child).clone());
                queue.push(child.pid);
            }
        }
    }
    out
}

/// This process, its ancestors and its descendants, which nothing here may
/// kill: on Windows a deploy run by an agent starts the dashboard below it.
fn own_lineage(table: &[Proc], me: u32) -> HashSet<u32> {
    let parents: HashMap<u32, u32> = table.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut out: HashSet<u32> = descendants(table, &[me]).iter().map(|p| p.pid).collect();
    let mut at = me;
    while out.insert(at) {
        match parents.get(&at) {
            Some(&parent) if parent != at => at = parent,
            _ => break,
        }
    }
    out
}

/// The pids among `before` still running under the same image in `now`, so a
/// pid the system has since handed to another program is left alone.
fn survivors(before: &[Proc], now: &[Proc]) -> HashSet<u32> {
    let images: HashMap<u32, &str> = now.iter().map(|p| (p.pid, p.image.as_str())).collect();
    before.iter().filter(|p| images.get(&p.pid) == Some(&p.image.as_str())).map(|p| p.pid).collect()
}

/// The kills among `refused` whose process is still running in `after`, judged
/// by [`survivors`] against the image `before` showed, so a pid handed to
/// another program since counts as stopped. Every refusal is kept where `after`
/// could not be read, and so is one for a pid `before` did not list, which
/// leaves nothing to tell its process from a stranger.
fn still_running(refused: Vec<(u32, String)>, before: &[Proc], after: Option<&[Proc]>) -> Vec<(u32, String)> {
    let Some(after) = after else { return refused };
    let pids: HashSet<u32> = refused.iter().map(|(pid, _)| *pid).collect();
    let known: Vec<Proc> = before.iter().filter(|p| pids.contains(&p.pid)).cloned().collect();
    let live = survivors(&known, after);
    refused.into_iter().filter(|(pid, _)| live.contains(pid) || !known.iter().any(|p| p.pid == *pid)).collect()
}

/// The pids listening on TCP `port` on this machine.
fn listening_pids(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    let cmd = {
        let mut c = Command::new("netstat");
        c.arg("-ano");
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = Command::new("/usr/sbin/lsof");
        c.args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"]);
        c
    };
    let Ok(out) = crate::subprocess::run(cmd, KILL_TIMEOUT) else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    #[cfg(windows)]
    return netstat_listeners(&text, port);
    #[cfg(not(windows))]
    return text.lines().filter_map(|l| l.trim().parse().ok()).collect();
}

/// The pids `netstat -ano` lists as listening on TCP `port`, on any address.
#[cfg_attr(not(windows), allow(dead_code))]
fn netstat_listeners(text: &str, port: u16) -> Vec<u32> {
    let suffix = format!(":{port}");
    let mut pids: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            match cols.as_slice() {
                ["TCP", local, _, "LISTENING", pid] if local.ends_with(&suffix) => pid.parse().ok(),
                _ => None,
            }
        })
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// End one process: `taskkill /F` on Windows, with `/T` for its tree where
/// `tree`, and `SIGTERM` elsewhere, where the caller lists a tree itself.
fn kill(pid: u32, tree: bool) -> Result<(), String> {
    #[cfg(windows)]
    let cmd = {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/F"]);
        if tree {
            c.arg("/T");
        }
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let _ = tree;
        let mut c = Command::new("/bin/kill");
        c.args(["-TERM", &pid.to_string()]);
        c
    };
    crate::subprocess::run_checked(cmd, KILL_TIMEOUT).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, image: &str) -> Proc {
        Proc { pid, ppid, image: image.into() }
    }

    #[test]
    fn descendants_walk_the_whole_tree_below_the_roots_and_nothing_else() {
        let table = [p(1, 0, "launchd"), p(10, 1, "claude"), p(11, 10, "zsh"), p(12, 11, "node"), p(13, 12, "esbuild"), p(20, 1, "other"), p(21, 20, "child"), p(0, 0, "idle")];
        let mut found: Vec<u32> = descendants(&table, &[10]).iter().map(|p| p.pid).collect();
        found.sort_unstable();
        assert_eq!(found, [11, 12, 13]);
        assert!(descendants(&table, &[]).is_empty());
    }

    #[test]
    fn the_dashboard_its_ancestors_and_its_children_are_spared() {
        // A deploy run by the agent (10) started the dashboard (14) under a shell.
        let table = [p(1, 0, "launchd"), p(10, 1, "claude"), p(11, 10, "bash"), p(14, 11, "dashboard"), p(15, 14, "lsof"), p(12, 10, "node")];
        assert_eq!(own_lineage(&table, 14), HashSet::from([14, 15, 11, 10, 1, 0]));
        let reachable: HashSet<u32> = descendants(&table, &[10]).iter().map(|p| p.pid).filter(|pid| !own_lineage(&table, 14).contains(pid)).collect();
        assert_eq!(reachable, HashSet::from([12]));
    }

    #[test]
    fn an_exited_agent_keeps_its_last_descendants_until_it_leaves_its_row() {
        let tracked = Tracked::default();
        let members = HashSet::from([("web".to_string(), 10)]);
        tracked.record(&[p(10, 1, "claude"), p(11, 10, "node")], &members);
        // The agent exited: its child is orphaned, and the agent is gone from the table.
        tracked.record(&[p(11, 1, "node")], &members);
        assert_eq!(tracked.take("web"), [p(11, 10, "node")]);
        assert!(tracked.take("web").is_empty(), "taken once");
        tracked.record(&[p(10, 1, "claude"), p(11, 10, "node")], &members);
        tracked.record(&[], &HashSet::new());
        assert!(tracked.take("web").is_empty(), "a pid no longer a member is forgotten");
    }

    #[test]
    fn a_refused_kill_of_a_process_already_gone_is_no_failure() {
        let refused = vec![(7, "not found".to_string()), (8, "access denied".to_string()), (9, "not found".to_string())];
        let before = [p(7, 1, "node"), p(8, 1, "node"), p(9, 1, "node")];
        assert_eq!(still_running(refused.clone(), &before, Some(&[p(8, 1, "node"), p(9, 1, "Safari")])), [(8, "access denied".to_string())], "a pid now running another program was stopped");
        assert_eq!(still_running(refused.clone(), &before, None), refused, "an unread table keeps every refusal");
    }

    #[test]
    fn a_pid_reused_by_another_program_is_not_a_survivor() {
        let before = [p(11, 10, "zsh"), p(12, 11, "node"), p(13, 12, "esbuild")];
        let now = [p(12, 1, "node"), p(13, 1, "Safari")];
        assert_eq!(survivors(&before, &now), HashSet::from([12]));
    }

    #[test]
    fn netstat_lists_the_listeners_on_the_port_alone() {
        let text = "\nActive Connections\n\n  Proto  Local Address          Foreign Address        State           PID\n  TCP    0.0.0.0:5173           0.0.0.0:0              LISTENING       4242\n  TCP    [::]:5173              [::]:0                 LISTENING       4242\n  TCP    0.0.0.0:51730          0.0.0.0:0              LISTENING       7\n  TCP    127.0.0.1:5173         127.0.0.1:60000        ESTABLISHED     4242\n  UDP    0.0.0.0:5173           *:*                                    9\n";
        assert_eq!(netstat_listeners(text, 5173), [4242]);
        assert!(netstat_listeners(text, 3000).is_empty());
    }
}
