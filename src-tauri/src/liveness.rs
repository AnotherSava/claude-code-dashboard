//! Detecting Claude sessions that exited without telling the dashboard.
//!
//! Claude Code's `SessionEnd` hook is the clean "row removed" signal, but it
//! fires reliably only on `/clear` — not on typing `exit`, Ctrl-D, or closing
//! the terminal (a documented Claude Code limitation: on those paths the hook
//! either doesn't fire or is killed before its HTTP POST completes). When it
//! doesn't fire, the session's row is stranded in whatever state it last held —
//! typically `Working`, if the user exited mid-turn.
//!
//! The dashboard can't be handed the owning process id — no hook payload field
//! or env var exposes it — so the hook resolves it itself: it walks its own
//! ancestor chain and reports the nearest ancestor whose image is `claude`
//! (`claude.exe` on Windows) as `agent_pid`, fresh on every event. [`AgentPids`]
//! stores the latest per chat_id (overwrite, so a same-cwd restart's new pid
//! replaces the old one before the reaper can act on the stale one).
//! [`crate::liveness_reaper`] then watches those pids: once one is positively
//! confirmed gone, it removes the row exactly as a SessionEnd would.
//!
//! Liveness is **image-confirmed**, not bare existence: a pid counts as alive
//! only if it is present in a full process enumeration AND still carries a
//! claude image. An absent pid is dead; a pid reused by a non-claude process is
//! dead too (the claude we tracked is gone); and if the enumeration itself fails
//! we conclude nothing that tick. This defeats both pid reuse (a recycled pid
//! won't read as a live claude) and the access-denied trap (enumeration needs no
//! per-process handle, so an *elevated* claude is still visible) that a naive
//! `OpenProcess` check would hit.
//!
//! node-based installs (image `node`, not `claude`) don't resolve an
//! `agent_pid`, so the reaper simply no-ops for them — today's behavior. Native
//! `claude` binaries (the current default) are covered.

use std::collections::HashMap;
use std::sync::Mutex;

/// Latest owning Claude pid per chat_id, as reported by the hook on each event.
/// Overwrite semantics (not the intersection [`crate::terminal_title`] uses):
/// each event carries the *current* pid, so a session restarted in the same cwd
/// replaces a now-dead pid before the reaper can act on the stale one.
#[derive(Default)]
pub struct AgentPids {
    map: Mutex<HashMap<String, u32>>,
}

impl AgentPids {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, chat_id: &str, pid: u32) {
        self.map.lock().unwrap().insert(chat_id.to_string(), pid);
    }

    pub fn get(&self, chat_id: &str) -> Option<u32> {
        self.map.lock().unwrap().get(chat_id).copied()
    }

    pub fn forget(&self, chat_id: &str) {
        self.map.lock().unwrap().remove(chat_id);
    }
}

/// True if `image` names the Claude Code executable: basename, case-insensitive,
/// stem (sans `.exe`) equal to `claude`. Matches `claude.exe` / `claude` and a
/// full path to either; rejects `node`/`node.exe` and anything else.
pub fn is_claude_image(image: &str) -> bool {
    let lower = image.trim().to_ascii_lowercase();
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    let stem = base.strip_suffix(".exe").unwrap_or(base);
    stem == "claude"
}

/// A snapshot of running process ids → image name, or `None` if the OS
/// enumeration failed (the caller then concludes nothing this tick — never a
/// false "dead"). On Windows the image is the bare exe name (e.g. `claude.exe`);
/// on macOS it is the executable path from `ps`. Either way run it through
/// [`is_claude_image`].
#[cfg(windows)]
pub fn process_images() -> Option<HashMap<u32, String>> {
    // Hand-declared, same style as `terminal_title::push_title` — avoids a
    // `windows`/`windows-sys` dependency for a couple of ancient calls.
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut Pe32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut Pe32W) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }
    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const INVALID_HANDLE: isize = -1;

    // PROCESSENTRY32W, field-for-field.
    #[repr(C)]
    struct Pe32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE || snap == 0 {
            return None;
        }
        let mut map = HashMap::new();
        let mut entry: Pe32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<Pe32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            let len = entry.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(entry.sz_exe_file.len());
            map.insert(entry.th32_process_id, String::from_utf16_lossy(&entry.sz_exe_file[..len]));
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
        Some(map)
    }
}

#[cfg(not(windows))]
pub fn process_images() -> Option<HashMap<u32, String>> {
    let out = std::process::Command::new("ps").args(["-axo", "pid=,comm="]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid_str, comm)) = line.split_once(char::is_whitespace) else { continue };
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            map.insert(pid, comm.trim().to_string());
        }
    }
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_image_matches_only_the_claude_binary() {
        assert!(is_claude_image("claude.exe"));
        assert!(is_claude_image("CLAUDE.EXE"));
        assert!(is_claude_image("claude"));
        assert!(is_claude_image(r"C:\Users\me\AppData\Local\Programs\claude.exe"));
        assert!(is_claude_image("/usr/local/bin/claude"));
        assert!(!is_claude_image("node.exe"), "node-based install isn't matched");
        assert!(!is_claude_image("node"));
        assert!(!is_claude_image("claude-code.exe"), "a different binary name");
        assert!(!is_claude_image(""));
    }

    #[test]
    fn process_images_includes_self() {
        // A real enumeration on the test host must at least contain this process.
        let map = process_images().expect("process enumeration works on the test host");
        let me = std::process::id();
        assert!(map.contains_key(&me), "our own pid is in the snapshot");
        assert!(!map.get(&me).unwrap().is_empty(), "with a non-empty image name");
    }

    #[test]
    fn agent_pids_keeps_only_the_latest() {
        let p = AgentPids::new();
        p.set("a", 100);
        assert_eq!(p.get("a"), Some(100));
        p.set("a", 200); // a same-cwd restart reports the new pid
        assert_eq!(p.get("a"), Some(200));
        p.forget("a");
        assert_eq!(p.get("a"), None);
    }
}
