//! Per-client event adapters.
//!
//! Each external agent (Claude Code today, other agents later) speaks its own
//! event vocabulary. An adapter converts one raw event payload into a uniform
//! [`AdapterOutput`] that `http_server` then applies to `AppState`.
//!
//! Adding a new client = new submodule + a match arm in [`dispatch`]. The HTTP
//! route never changes.

use std::path::PathBuf;

use crate::config::Config;
use crate::state::SetInput;

pub mod claude;

#[derive(Debug)]
pub enum AdapterOutput {
    /// Apply a state update. `transcript_path`, when present, is handed to
    /// the log-watcher registry by the HTTP layer after the state update.
    Set {
        input: SetInput,
        transcript_path: Option<PathBuf>,
        /// Human-readable explanation of *why* this status/label was chosen.
        /// The HTTP layer writes it to the `decision` log in `widget.jsonl` so
        /// "why is this row in state X?" is answerable without the transcript
        /// or the code — it carries the matched question-rule and a text
        /// snippet for the question path.
        reason: String,
    },
    /// Remove a session.
    Clear { id: String },
    /// Mark a context boundary in the session's dialog (a history separator)
    /// without changing its status — e.g. on context compaction, the session
    /// continues but its prior dialog belongs to a different context window.
    Boundary { id: String },
    /// Adapter does not handle this event — drop silently. Useful for
    /// lifecycle events we subscribe to but don't need (future-proofing).
    Ignore,
}

/// Dispatch an incoming event to the correct adapter by client id.
pub fn dispatch(
    client: &str,
    event: &str,
    payload: &serde_json::Value,
    cfg: &Config,
) -> AdapterOutput {
    match client {
        "claude" => claude::dispatch(event, payload, cfg),
        _ => AdapterOutput::Ignore,
    }
}
