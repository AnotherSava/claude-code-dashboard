//! Keeping a project's dashboard data when its folder is renamed or moved.
//!
//! A row id is derived from the project's directory (`adapters::claude::derive_chat_id`),
//! so renaming `my-app` to `my-app-renamed` makes every store keyed by the id
//! point at a row that will never be written again: the history stays under the
//! old key, a resumed session's anchor keeps landing on the old row while new
//! sessions land on the new one, and a custom name or a start grant stops
//! applying. Nothing can tell a rename from a different project from the outside,
//! so the move is announced: `POST /api/project/rename` (`http_server`) runs
//! [`rename_project`], which re-keys every local store in one pass, and the
//! rename then rides every sync push ([`RenameLog`]) so a peer re-files its
//! stored copy of the history and its own name for the row.
//!
//! The route refuses while a session is live under any id it would move. A row
//! that still exists under the old id would be persisted under it again the
//! moment it ends (`commands::remove_session` saves the dialog under the row's
//! id), which would put the history right back where it was moved from.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::adapters::claude::derive_chat_id;
use crate::auto_start_store::AutoStartStore;
use crate::chat_id_registry::ChatIdRegistry;
use crate::commands::RowLocks;
use crate::config::ConfigState;
use crate::custom_names::CustomNamesStore;
use crate::prompt_history::PromptHistoryStore;
use crate::session_registry::SessionRegistry;
use crate::state::AppState;

/// How long a rename keeps riding the sync push. A peer that has been off for
/// longer misses it and pulls the history under the new id instead, which it
/// does unprompted, leaving its old copy behind unused.
const ANNOUNCE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What moving one map key to another did. Shared by every store keyed by a
/// row id, so a rename reports each store in the same terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMove {
    Moved,
    /// Nothing was stored under the old key.
    Absent,
    /// Both keys hold a value, so nothing moved.
    Conflict,
}

/// Move `from`'s value to `to` in a row-id-keyed map, refusing to overwrite.
pub fn rename_key<V>(map: &mut std::collections::HashMap<String, V>, from: &str, to: &str) -> KeyMove {
    if !map.contains_key(from) {
        return KeyMove::Absent;
    }
    if map.contains_key(to) {
        return KeyMove::Conflict;
    }
    let value = map.remove(from).expect("checked above");
    map.insert(to.to_string(), value);
    KeyMove::Moved
}

/// A path with `\` turned into `/` and no trailing slash, case untouched.
fn clean(p: &str) -> String {
    p.trim().replace('\\', "/").trim_end_matches('/').to_string()
}

/// Path-segment equality: exact, or ASCII-case-insensitive on Windows, whose
/// file system ignores case.
fn segments_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether two spellings name the same directory: separators and a trailing
/// slash are ignored, as `derive_chat_id` ignores them.
pub fn same_dir(a: &str, b: &str) -> bool {
    segments_eq(&clean(a), &clean(b))
}

/// Where `dir` lands when `old_root` moves to `new_root`: `Some` for the root
/// itself and anything beneath it, `None` for anything else.
fn relocate(dir: &str, old_root: &str, new_root: &str) -> Option<String> {
    let (d, root, new_root) = (clean(dir), clean(old_root), clean(new_root));
    if segments_eq(&d, &root) {
        return Some(new_root);
    }
    let n = root.len();
    let inside = d.len() > n && d.is_char_boundary(n) && segments_eq(&d[..n], &root) && d[n..].starts_with('/');
    inside.then(|| format!("{new_root}{}", &d[n..]))
}

/// One directory the move carries along, with the ids it derives before and
/// after.
#[derive(Debug, Clone, PartialEq)]
pub struct DirMove {
    pub old_dir: String,
    pub new_dir: String,
    pub from: String,
    pub to: String,
}

/// Every directory a move of `old_path` to `new_path` relocates: the folder
/// itself, plus each known directory beneath it (`known` is Claude Code's
/// project index and the start grants). A subfolder needs its own entry because
/// with `projects_root` set its id contains the parent's name, so it changes
/// too; with `projects_root` unset its id is its own name and stays, which the
/// caller sees as `from == to`.
pub fn plan_moves(old_path: &str, new_path: &str, known: &[String], projects_root: Option<&str>) -> Vec<DirMove> {
    let mut dirs = vec![old_path.to_string()];
    for d in known {
        if !dirs.iter().any(|seen| same_dir(seen, d)) && relocate(d, old_path, new_path).is_some() {
            dirs.push(d.clone());
        }
    }
    dirs.into_iter()
        .filter_map(|old_dir| {
            let new_dir = relocate(&old_dir, old_path, new_path)?;
            let from = derive_chat_id(Some(&old_dir), projects_root);
            let to = derive_chat_id(Some(&new_dir), projects_root);
            Some(DirMove { old_dir, new_dir, from, to })
        })
        .collect()
}

/// One rename as it rides the sync push: raw row ids on the sending device,
/// which the receiver namespaces with that device's name.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProjectRename {
    pub from: String,
    pub to: String,
    /// When it was recorded, on the sender's clock, strictly increasing within
    /// one sender's log. A receiver applies each rename once, in this order,
    /// and remembers the newest it applied, so a rename is never re-applied to
    /// an id that has since come back into use.
    pub at: i64,
}

/// The renames this dashboard still announces to its peers, oldest first, in
/// `project_renames.json` so a restart inside the window keeps announcing them.
pub struct RenameLog {
    path: PathBuf,
    data: Mutex<Vec<ProjectRename>>,
}

impl RenameLog {
    pub fn new(path: PathBuf) -> Self {
        let data = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
                tracing::warn!(?e, path = %path.display(), "failed to parse project renames");
                Vec::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "failed to read project renames");
                Vec::new()
            }
        };
        Self { path, data: Mutex::new(data) }
    }

    pub fn record(&self, from: &str, to: &str, now: i64) {
        let mut data = self.data.lock().unwrap();
        data.retain(|r| now - r.at < ANNOUNCE_MS);
        let at = data.last().map_or(now, |last| now.max(last.at + 1));
        data.push(ProjectRename { from: from.to_string(), to: to.to_string(), at });
        match serde_json::to_string_pretty(&*data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    tracing::warn!(?e, path = %self.path.display(), "failed to write project renames");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize project renames"),
        }
    }

    /// The renames still inside the announcement window, oldest first.
    pub fn recent(&self, now: i64) -> Vec<ProjectRename> {
        self.data.lock().unwrap().iter().filter(|r| now - r.at < ANNOUNCE_MS).cloned().collect()
    }
}

/// What one id's move did, store by store.
#[derive(Serialize, Debug)]
pub struct IdReport {
    pub from: String,
    pub to: String,
    pub history: KeyMove,
    pub custom_name: KeyMove,
    /// Sessions whose anchor now points at the new id.
    pub anchors: usize,
}

/// What `POST /api/project/rename` answers with, so the caller can see what
/// moved rather than only that something did.
#[derive(Serialize, Debug)]
pub struct RenameReport {
    /// One entry per id that changed: the folder's own, then any subfolder's.
    pub ids: Vec<IdReport>,
    /// Start grants re-pointed at the new location, whether or not their id changed.
    pub start_grants: usize,
    /// Rows whose session had ended without telling the dashboard, removed so
    /// their dialog could move.
    pub cleared_rows: usize,
    /// Whether Claude Code's live-session list could be read. Where it could
    /// not, no row could be shown to have ended, so the call went ahead only
    /// because none was present.
    pub live_sessions_checked: bool,
}

/// Why a rename was not carried out. Nothing has been changed in any of them.
#[derive(Debug, PartialEq)]
pub enum RenameRefusal {
    EmptyPath,
    /// The folder is not where the call says: `new_path` is not a directory, or
    /// `old_path` still exists. Usually the move itself failed.
    NotMoved(String),
    /// Claude Code's project index could not be read, and without it neither
    /// the subfolders that move along nor a folder sharing the id can be found.
    IndexUnreadable,
    /// A Claude Code session that is still running holds this id.
    Live(String),
    /// Both ids already hold history. Happens when another project derives the
    /// new id, and merging two projects' dialogs cannot be undone.
    HistoryConflict(String),
    /// Another folder that still exists derives the old id, so its history is
    /// the same row and moving it would take it from that folder too.
    SharedId { id: String, dir: String },
    /// One id would be both moved away and moved onto, in an order no sequence
    /// of moves can satisfy without one carrying the other's data.
    Tangled(String),
}

impl RenameRefusal {
    pub fn detail(&self) -> String {
        match self {
            Self::EmptyPath => "old_path and new_path are both required".into(),
            Self::NotMoved(why) => format!("{why}; move the folder first, then send this again"),
            Self::IndexUnreadable => "Claude Code's project index (~/.claude.json) could not be read; send this again in a moment".into(),
            Self::Live(id) => format!("a Claude Code session for \"{id}\" is still running; exit it and send this again"),
            Self::HistoryConflict(id) => format!("\"{id}\" already has history of its own, so the two cannot be combined; retrying will not change that"),
            Self::SharedId { id, dir } => format!("{dir} also derives \"{id}\" and shares its history, so moving it would take that folder's history too; rename or remove {dir}, then send this again"),
            Self::Tangled(id) => format!("\"{id}\" would be both moved away and moved onto by this rename"),
        }
    }
}

/// Put the moves in an order where every id is moved away before another move
/// lands on it: deepest directory first, since under a `projects_root` a
/// parent's new id can be a subfolder's old one (`bga` → `bga-assistant` beside
/// `bga/assistant`). Refuses where a move would land on an id a later move
/// still has to carry away.
fn order_moves(mut moves: Vec<DirMove>) -> Result<Vec<DirMove>, RenameRefusal> {
    moves.sort_by_key(|m| std::cmp::Reverse(clean(&m.old_dir).matches('/').count()));
    for (i, m) in moves.iter().enumerate() {
        if m.from != m.to && moves[i + 1..].iter().any(|later| later.from != later.to && later.from == m.to) {
            return Err(RenameRefusal::Tangled(m.to.clone()));
        }
    }
    Ok(moves)
}

/// Whether `new_path` holds the folder now. `old_path` still existing refuses,
/// except where it is the same directory under a different case, which a
/// case-insensitive file system (Windows, macOS by default) still reports as
/// existing after a case-only rename.
fn check_moved(old_path: &str, new_path: &str) -> Result<(), RenameRefusal> {
    if !Path::new(new_path).is_dir() {
        return Err(RenameRefusal::NotMoved(format!("{new_path} is not a directory")));
    }
    if !Path::new(old_path).exists() {
        return Ok(());
    }
    let canonical_new = std::fs::canonicalize(new_path).ok();
    let same = canonical_new.is_some() && std::fs::canonicalize(old_path).ok() == canonical_new;
    let on_disk = canonical_new.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
    let wanted = clean(new_path).rsplit('/').next().map(str::to_string);
    if same && on_disk == wanted {
        return Ok(());
    }
    Err(RenameRefusal::NotMoved(format!("{old_path} still exists")))
}

/// Whether a row's session is known to be running: Claude Code's live-session
/// list names it, or one of the row's members is still a Claude Code process. A
/// list that could not be read counts as running; a member with no pid cannot
/// be checked and does not.
fn row_is_live(app: &AppHandle, id: &str, live_ids: Option<&[String]>) -> bool {
    let Some(live_ids) = live_ids else { return true };
    if live_ids.iter().any(|l| l == id) {
        return true;
    }
    let pids = app.try_state::<crate::membership::Members>().map(|m| m.pids(id)).unwrap_or_default();
    if pids.is_empty() {
        return false;
    }
    match crate::liveness::process_images() {
        Some(images) => pids.iter().any(|&pid| crate::liveness::is_live_claude(&images, pid)),
        None => true,
    }
}

/// Re-key everything this dashboard keeps for the project at `old_path`, and for
/// any known subfolder of it, to the ids they derive at `new_path`, and start
/// announcing the renames to peers. Call after the folder has moved.
///
/// A row still present under a moving id whose session has ended is removed
/// first, the way an exit would remove it, which saves its dialog so the move
/// carries it. Claude Code often sends no `SessionEnd` on exit and the liveness
/// reaper needs seconds to notice, or never does for a session whose process it
/// never learned, so without this the call usually arrives before the row goes.
pub fn rename_project(app: &AppHandle, old_path: &str, new_path: &str, now: i64) -> Result<RenameReport, RenameRefusal> {
    if old_path.trim().is_empty() || new_path.trim().is_empty() {
        return Err(RenameRefusal::EmptyPath);
    }
    // The paths are the caller's word, and a move that failed would otherwise
    // file the history under a name no folder derives.
    check_moved(old_path, new_path)?;
    let projects_root = app.try_state::<ConfigState>().and_then(|c| c.snapshot().projects_root);
    let grants = app.try_state::<AutoStartStore>();
    let mut known = crate::session_launcher::indexed_dirs().ok_or(RenameRefusal::IndexUnreadable)?;
    known.extend(grants.iter().flat_map(|g| g.snapshot().into_values()));
    let moves = order_moves(plan_moves(old_path, new_path, &known, projects_root.as_deref()))?;
    let renamed: Vec<&DirMove> = moves.iter().filter(|m| m.from != m.to).collect();

    // Another existing folder deriving an id we would move shares its history.
    // Both spellings of the folder itself are skipped, since after a case-only
    // rename the old one still resolves.
    for m in &renamed {
        let other = known.iter().find(|d| !same_dir(d, old_path) && !same_dir(d, new_path) && derive_chat_id(Some(d), projects_root.as_deref()) == m.from && Path::new(d).is_dir());
        if let Some(other) = other {
            return Err(RenameRefusal::SharedId { id: m.from.clone(), dir: other.clone() });
        }
    }

    // Held for the whole pass, in a fixed order, so a hook event cannot create
    // a row between the live check and the moves.
    let mut ids: Vec<&str> = renamed.iter().flat_map(|m| [m.from.as_str(), m.to.as_str()]).collect();
    ids.sort_unstable();
    ids.dedup();
    let locks = app.try_state::<RowLocks>();
    let rows: Vec<_> = locks.iter().flat_map(|l| ids.iter().map(|id| l.row(id))).collect();
    let _guards: Vec<_> = rows.iter().map(|r| RowLocks::hold(r)).collect();

    let registry = app.try_state::<ChatIdRegistry>();
    let anchored = |sid: &str| registry.as_ref().and_then(|r| r.anchored(sid));
    let live_ids: Option<Vec<String>> = app
        .try_state::<SessionRegistry>()
        .and_then(|r| {
            r.invalidate();
            r.live_sessions(projects_root.as_deref(), now)
        })
        .map(|live| live.iter().flat_map(|s| [s.chat_id.clone(), s.row_id(&anchored)]).collect());
    if let Some(id) = live_ids.iter().flatten().find(|l| ids.contains(&l.as_str())) {
        return Err(RenameRefusal::Live(id.clone()));
    }
    let present: Vec<String> = app
        .try_state::<AppState>()
        .map(|s| s.sessions.lock().unwrap().iter().filter(|r| ids.contains(&r.id.as_str())).map(|r| r.id.clone()).collect())
        .unwrap_or_default();
    if let Some(id) = present.iter().find(|id| row_is_live(app, id, live_ids.as_deref())) {
        return Err(RenameRefusal::Live(id.clone()));
    }

    // Checked in the order the moves run, so an id an earlier move vacated is
    // free for a later one. A row about to be cleared counts as history, since
    // clearing saves its dialog under its id.
    let history = app.try_state::<PromptHistoryStore>();
    if let Some(h) = &history {
        let mut held: Vec<String> = ids.iter().filter(|id| h.get(id).is_some() || present.iter().any(|p| p == *id)).map(|id| id.to_string()).collect();
        for m in &renamed {
            if held.contains(&m.from) {
                if held.contains(&m.to) {
                    return Err(RenameRefusal::HistoryConflict(m.to.clone()));
                }
                held.retain(|id| id != &m.from);
                held.push(m.to.clone());
            }
        }
    }

    for id in &present {
        if crate::commands::remove_session(app, id, crate::state::BoundaryKind::Ended, now, crate::membership::Membership::Forget) {
            tracing::info!(chat_id = %id, decision = "rename_cleared_row", "a project rename removed a row whose session had ended without telling the dashboard");
        }
    }
    let names = app.try_state::<CustomNamesStore>();
    let log = app.try_state::<RenameLog>();
    let mut reports = Vec::new();
    for m in &renamed {
        reports.push(IdReport {
            from: m.from.clone(),
            to: m.to.clone(),
            history: history.as_ref().map_or(KeyMove::Absent, |h| h.rename(&m.from, &m.to)),
            custom_name: names.as_ref().map_or(KeyMove::Absent, |n| n.rename(&m.from, &m.to)),
            anchors: registry.as_ref().map_or(0, |r| r.retarget(&m.from, &m.to)),
        });
        if let Some(log) = &log {
            log.record(&m.from, &m.to, now);
        }
    }
    let start_grants = grants.as_ref().map_or(0, |g| moves.iter().filter(|m| g.move_grant(&m.from, &m.old_dir, &m.to, &m.new_dir) == KeyMove::Moved).count());
    if !renamed.is_empty() {
        if let Some(dirty) = app.try_state::<crate::sync::SyncDirty>() {
            dirty.0.notify_one();
        }
    }

    let report = RenameReport { ids: reports, start_grants, cleared_rows: present.len(), live_sessions_checked: live_ids.is_some() };
    for r in &report.ids {
        tracing::info!(
            chat_id = %r.to,
            decision = "project_rename",
            from = %r.from,
            history = ?r.history,
            custom_name = ?r.custom_name,
            anchors = r.anchors,
            live_sessions_checked = report.live_sessions_checked,
            "project folder renamed; its dashboard data now lives under the new id"
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn rename_key_moves_a_value_and_never_overwrites() {
        let mut map: HashMap<String, u32> = HashMap::from([("old".into(), 1), ("other".into(), 2)]);
        assert_eq!(rename_key(&mut map, "old", "new"), KeyMove::Moved);
        assert_eq!(map.get("new"), Some(&1));
        assert!(!map.contains_key("old"));
        assert_eq!(rename_key(&mut map, "old", "new"), KeyMove::Absent, "a repeat finds nothing left to move");
        assert_eq!(rename_key(&mut map, "new", "other"), KeyMove::Conflict);
        assert_eq!((map.get("new"), map.get("other")), (Some(&1), Some(&2)), "a conflict leaves both alone");
    }

    #[test]
    fn same_dir_ignores_separators_and_a_trailing_slash() {
        assert!(same_dir("C:\\src\\a\\", "C:/src/a"));
        assert!(!same_dir("/p/a/web", "/p/b/web"));
    }

    #[test]
    fn relocate_keeps_the_subpath_and_nothing_outside_the_folder() {
        assert_eq!(relocate("/p/bga", "/p/bga", "/p/games").as_deref(), Some("/p/games"));
        assert_eq!(relocate("/p/bga/Assistant/", "/p/bga", "/p/games").as_deref(), Some("/p/games/Assistant"));
        assert_eq!(relocate("/p/bga-tools", "/p/bga", "/p/games"), None, "a sibling sharing the prefix is not inside it");
    }

    #[test]
    fn a_plain_rename_moves_the_folders_own_id() {
        let moves = plan_moves("C:/src/my-app", "C:/src/my-app-renamed", &[], None);
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].from.as_str(), moves[0].to.as_str()), ("my-app", "my-app-renamed"));
    }

    #[test]
    fn under_a_projects_root_a_subfolders_id_moves_with_its_parent() {
        let known = vec!["/p/bga/assistant".to_string(), "/p/bga-tools".to_string(), "/p/other".to_string()];
        let moves = plan_moves("/p/bga", "/p/games", &known, Some("/p"));
        let ids: Vec<_> = moves.iter().map(|m| (m.from.as_str(), m.to.as_str())).collect();
        assert_eq!(ids, [("bga", "games"), ("bga assistant", "games assistant")]);
    }

    #[test]
    fn without_a_projects_root_a_subfolder_keeps_its_id_and_only_its_path_moves() {
        let known = vec!["/p/bga/assistant".to_string()];
        let moves = plan_moves("/p/bga", "/p/games", &known, None);
        assert_eq!(moves[1].from, moves[1].to, "its id is its own name");
        assert_eq!(moves[1].new_dir, "/p/games/assistant", "but a start grant still needs the new path");
    }

    /// `bga` → `bga-assistant` beside `bga/assistant`: the parent's new id is
    /// the subfolder's old one, so the subfolder has to move out first.
    #[test]
    fn a_subfolder_is_moved_before_its_parent_lands_on_its_id() {
        let known = vec!["/p/bga/assistant".to_string()];
        let moves = order_moves(plan_moves("/p/bga", "/p/bga-assistant", &known, Some("/p"))).expect("an order exists");
        let ids: Vec<_> = moves.iter().map(|m| (m.from.as_str(), m.to.as_str())).collect();
        assert_eq!(ids, [("bga assistant", "bga assistant assistant"), ("bga", "bga assistant")]);
    }

    #[test]
    fn a_move_landing_on_an_id_still_to_be_carried_away_is_refused() {
        let mv = |old_dir: &str, from: &str, to: &str| DirMove { old_dir: old_dir.into(), new_dir: String::new(), from: from.into(), to: to.into() };
        let tangled = vec![mv("/p/a/b", "x", "y"), mv("/p/c/d", "y", "z")];
        assert_eq!(order_moves(tangled), Err(RenameRefusal::Tangled("y".into())));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("project_rename_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_rename_is_accepted_only_once_the_folder_has_moved() {
        let root = scratch("moved");
        let (old, new) = (root.join("old").to_string_lossy().into_owned(), root.join("new").to_string_lossy().into_owned());
        std::fs::create_dir(&old).unwrap();
        assert!(matches!(check_moved(&old, &new), Err(RenameRefusal::NotMoved(_))), "not moved yet");
        std::fs::create_dir(&new).unwrap();
        assert!(matches!(check_moved(&old, &new), Err(RenameRefusal::NotMoved(_))), "both exist: the move failed or was a copy");
        std::fs::remove_dir(&old).unwrap();
        assert_eq!(check_moved(&old, &new), Ok(()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A case-insensitive file system still finds the old spelling after a
    /// case-only rename; what decides it is the name actually on disk.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn a_case_only_rename_counts_as_moved() {
        let root = scratch("case");
        let (old, new) = (root.join("Web").to_string_lossy().into_owned(), root.join("web").to_string_lossy().into_owned());
        std::fs::create_dir(&old).unwrap();
        assert!(matches!(check_moved(&old, &new), Err(RenameRefusal::NotMoved(_))), "on disk it is still Web");
        std::fs::rename(&old, &new).unwrap();
        assert_eq!(check_moved(&old, &new), Ok(()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_log_keeps_every_rename_in_order_with_a_strictly_increasing_stamp() {
        let path = std::env::temp_dir().join(format!("project_renames_order_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = RenameLog::new(path.clone());
        log.record("web", "web-old", 1_000);
        log.record("web-new", "web", 1_000);
        let recent = log.recent(1_000);
        let pairs: Vec<_> = recent.iter().map(|r| (r.from.as_str(), r.to.as_str(), r.at)).collect();
        assert_eq!(pairs, [("web", "web-old", 1_000), ("web-new", "web", 1_001)], "a swap keeps both steps, so a receiver can replay them in order");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_log_survives_a_restart_and_ages_out() {
        let path = std::env::temp_dir().join(format!("project_renames_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        RenameLog::new(path.clone()).record("my-app", "my-app-renamed", 1_000);
        let log = RenameLog::new(path.clone());
        assert_eq!(log.recent(1_000 + ANNOUNCE_MS - 1).len(), 1);
        assert!(log.recent(1_000 + ANNOUNCE_MS).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
