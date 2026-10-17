//! When the previous run of this dashboard was last known to be alive.
//!
//! A session whose process started after that instant fired its `SessionStart`
//! into a dashboard that was not listening, so `session_restore` can put its row
//! back as the missed event would have created it, with no title to read. That
//! is the case a reboot produces on Windows, where every tab title lived on a
//! console object the reboot destroyed, while the terminal reopens its tabs and
//! starts their sessions before this app is up.
//!
//! A heartbeat rather than a stamp written on exit, because the tray's quit
//! calls `std::process::exit` and a crash or a power cut writes nothing. The
//! interval bounds the one way this errs: a session started during the last
//! [`HEARTBEAT_MS`] of the previous run reads as missed although that run saw
//! it, which restores a row that run may have removed in those seconds.
//!
//! Its own file for the reason `custom_names.json` has one: deploy overwrites
//! `config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::commands::now_ms;

const HEARTBEAT_MS: u64 = 30_000;

#[derive(Serialize, Deserialize)]
struct StampFile {
    at: i64,
}

pub struct AliveStamp {
    path: PathBuf,
    /// Read once at construction, before this run's first heartbeat replaces it.
    previous: Option<i64>,
}

impl AliveStamp {
    pub fn new(path: PathBuf) -> Self {
        // A missing or unreadable file is "unknown", which restores nothing on
        // this evidence: guessing an instant would mint rows on a guess.
        let previous = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<StampFile>(&s).ok()).map(|f| f.at);
        tracing::debug!(previous = ?previous, "previous run's last alive stamp loaded");
        Self { path, previous }
    }

    /// The previous run's last heartbeat, epoch ms on this machine's clock.
    pub fn previous(&self) -> Option<i64> {
        self.previous
    }

    /// Rewrite the stamp now and every [`HEARTBEAT_MS`] for the life of the process.
    pub fn spawn_heartbeat(&self) {
        let path = self.path.clone();
        std::thread::spawn(move || loop {
            let body = serde_json::to_string(&StampFile { at: now_ms() }).expect("a struct of one integer serializes");
            if let Err(e) = std::fs::write(&path, body) {
                tracing::warn!(error = %e, path = %path.display(), "could not write the alive stamp");
            }
            std::thread::sleep(std::time::Duration::from_millis(HEARTBEAT_MS));
        });
    }
}
