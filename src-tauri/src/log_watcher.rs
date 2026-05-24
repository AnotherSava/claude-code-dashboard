use crate::commands::{emit_sessions_updated, now_ms};
use crate::state::{AgentSession, AppState, Status};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

/// Block-level output of one inference pass over transcript lines. Fields are
/// `None` when the scan found nothing conclusive for that dimension — callers
/// are expected to preserve prior values rather than clobber to None.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InferredState {
    pub state: Option<Status>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
}

/// Walk JSONL lines newest-first and derive current state, last-known model,
/// and last-known token count from assistant `usage` blocks.
pub fn infer_state(lines: &[&str]) -> Option<InferredState> {
    let mut result = InferredState::default();
    let mut saw_conversational = false;

    for line in lines.iter().rev() {
        let entry: TranscriptEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.entry_type != "user" && entry.entry_type != "assistant" {
            continue;
        }
        let message = match entry.message {
            Some(m) => m,
            None => continue,
        };
        let content = match message.content {
            Some(c) => c,
            None => continue,
        };
        if content.is_empty() {
            continue;
        }
        saw_conversational = true;

        // Model + usage come only from main-session (non-sidechain) assistant
        // entries from a real Claude model. Sidechains (Task sub-agents) have
        // their own context windows, and synthetic error entries have a
        // non-claude model name — both would pollute the dashboard.
        if entry.entry_type == "assistant" && !entry.is_sidechain {
            if result.model.is_none() {
                if let Some(ref m) = message.model {
                    if m.starts_with("claude-") {
                        result.model = Some(m.clone());
                    }
                }
            }
            if result.input_tokens.is_none() {
                if let Some(ref usage) = message.usage {
                    let input = usage.input_tokens.unwrap_or(0);
                    let cc = usage.cache_creation_input_tokens.unwrap_or(0);
                    let cr = usage.cache_read_input_tokens.unwrap_or(0);
                    if input > 0 || cc > 0 || cr > 0 {
                        result.input_tokens = Some(input + cc + cr);
                    }
                }
            }
        }

        if result.state.is_none() {
            let has_tool_use = content.iter().any(|b| b.block_type == "tool_use");
            let has_tool_result = content.iter().any(|b| b.block_type == "tool_result");
            let has_text = content.iter().any(|b| {
                b.block_type == "text"
                    && b.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false)
            });
            if has_tool_use || has_tool_result {
                result.state = Some(Status::Working);
            } else if entry.entry_type == "user" && has_text {
                result.state = Some(Status::Working);
            } else if entry.entry_type == "assistant" && has_text {
                result.state = Some(Status::Done);
            }
        }

        if result.state.is_some() && result.model.is_some() && result.input_tokens.is_some() {
            break;
        }
    }

    if !saw_conversational && result.state.is_none() && result.model.is_none() && result.input_tokens.is_none() {
        return None;
    }
    Some(result)
}

/// Split a JSONL chunk on newlines, returning complete lines and the trailing
/// partial line (possibly empty) as the new `leftover` for the next chunk.
pub fn split_complete(leftover: &str, chunk: &str) -> (Vec<String>, String) {
    let combined = format!("{leftover}{chunk}");
    let Some(last_nl) = combined.rfind('\n') else {
        return (Vec::new(), combined);
    };
    let (complete, rest) = combined.split_at(last_nl);
    let leftover = rest[1..].to_string(); // drop the newline
    let lines: Vec<String> = complete
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect();
    (lines, leftover)
}

/// Upgrade-only merge policy. Watcher can set status to `working`, and can
/// update model / input_tokens. It cannot set terminal states (done, idle,
/// awaiting, error) — those are hook-authoritative. Returns true if anything
/// actually changed.
pub fn apply_watcher_update(
    session: &mut AgentSession,
    update: &InferredState,
    now_ms: i64,
) -> bool {
    let mut changed = false;
    if let Some(Status::Working) = update.state {
        if session.status != Status::Working {
            session.status = Status::Working;
            session.state_entered_at = now_ms;
            changed = true;
        }
    }
    if let Some(ref m) = update.model {
        if session.model.as_ref() != Some(m) {
            session.model = Some(m.clone());
            changed = true;
        }
    }
    if let Some(t) = update.input_tokens {
        if session.input_tokens != Some(t) {
            session.input_tokens = Some(t);
            changed = true;
        }
    }
    if changed {
        session.updated = now_ms;
    }
    changed
}

// -------- Wire types for deserializing JSONL entries --------

#[derive(Deserialize)]
struct TranscriptEntry {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    model: Option<String>,
    usage: Option<TranscriptUsage>,
    content: Option<Vec<TranscriptBlock>>,
}

#[derive(Deserialize)]
struct TranscriptBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct TranscriptUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

// -------- Watcher registry --------

#[derive(Default)]
pub struct WatcherRegistry {
    entries: Mutex<HashMap<String, WatchTask>>, // keyed by chat_id
}

struct WatchTask {
    path: PathBuf,
    abort: tauri::async_runtime::JoinHandle<()>,
}

impl WatcherRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotent. If `chat_id` already watches `path`, no-op. If it watches a
    /// different path, stop the old watcher first.
    pub fn start(&self, app: AppHandle, chat_id: String, path: PathBuf) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries.get(&chat_id) {
            if existing.path == path {
                return;
            }
            existing.abort.abort();
        }
        let id_for_task = chat_id.clone();
        let path_for_task = path.clone();
        let handle = tauri::async_runtime::spawn(async move {
            watch_loop(app, id_for_task, path_for_task).await;
        });
        entries.insert(
            chat_id,
            WatchTask {
                path,
                abort: handle,
            },
        );
    }

    pub fn stop(&self, chat_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(task) = entries.remove(chat_id) {
            task.abort.abort();
        }
    }
}

async fn watch_loop(app: AppHandle, chat_id: String, path: PathBuf) {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => {
            tracing::warn!(path = %path.display(), chat_id, "transcript path has no parent dir");
            return;
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watched = path.clone();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                return;
            }
            if event.paths.iter().any(|p| p == &watched) {
                let _ = tx.send(());
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "transcript watcher create failed");
            return;
        }
    };
    if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
        tracing::error!(parent = %parent.display(), error = %e, "transcript watch failed");
        return;
    }
    tracing::debug!(path = %path.display(), chat_id, "watching transcript");

    let state = Arc::new(Mutex::new(DrainState {
        position: 0,
        leftover: String::new(),
        initial_read: true,
    }));

    // Initial drain — the transcript usually exists already with prior turns.
    drain(&app, &chat_id, &path, &state).await;

    while let Some(()) = rx.recv().await {
        drain(&app, &chat_id, &path, &state).await;
    }
}

struct DrainState {
    position: u64,
    leftover: String,
    initial_read: bool,
}

async fn drain(app: &AppHandle, chat_id: &str, path: &Path, state: &Arc<Mutex<DrainState>>) {
    let (mut position, mut leftover, initial_read) = {
        let s = state.lock().unwrap();
        (s.position, s.leftover.clone(), s.initial_read)
    };

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let file_size = match file.metadata().map(|m| m.len()) {
        Ok(s) => s,
        Err(_) => return,
    };
    // File was truncated/rotated: restart from 0.
    if file_size < position {
        position = 0;
        leftover.clear();
    }
    if file_size == position {
        return;
    }
    if file.seek(SeekFrom::Start(position)).is_err() {
        return;
    }
    let mut chunk = String::new();
    if file.read_to_string(&mut chunk).is_err() {
        return;
    }

    let (lines, new_leftover) = split_complete(&leftover, &chunk);
    {
        let mut s = state.lock().unwrap();
        s.position = file_size;
        s.leftover = new_leftover;
    }

    if lines.is_empty() {
        return;
    }
    let borrowed: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let Some(mut update) = infer_state(&borrowed) else {
        return;
    };

    // Suppress state on the initial read — the hook owns current status
    // (e.g. `idle` on SessionStart), while a past assistant text in the
    // transcript would otherwise roll us back to `done`.
    if initial_read {
        state.lock().unwrap().initial_read = false;
        update.state = None;
    }

    apply_and_emit(app, chat_id, &update);
}

fn apply_and_emit(app: &AppHandle, chat_id: &str, update: &InferredState) {
    let Some(app_state) = app.try_state::<AppState>() else {
        return;
    };
    let changed = {
        let mut sessions = app_state.sessions.lock().unwrap();
        match sessions.iter_mut().find(|s| s.id == chat_id) {
            Some(session) => apply_watcher_update(session, update, now_ms()),
            None => false,
        }
    };
    if changed {
        emit_sessions_updated(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_text(text: &str) -> String {
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] }
        })
        .to_string()
    }

    fn assistant_text(text: &str) -> String {
        json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] }
        })
        .to_string()
    }

    fn assistant_tool_use() -> String {
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{ "type": "tool_use", "name": "Read" }]
            }
        })
        .to_string()
    }

    fn user_tool_result() -> String {
        json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "tool_result", "content": "ok" }]
            }
        })
        .to_string()
    }

    fn meta(entry_type: &str) -> String {
        json!({ "type": entry_type }).to_string()
    }

    fn assistant_with_usage(model: &str, input: u64, cc: u64, cr: u64) -> String {
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": model,
                "content": [{ "type": "text", "text": "answer" }],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": cc,
                    "cache_read_input_tokens": cr,
                }
            }
        })
        .to_string()
    }

    fn refs<'a>(v: &'a [String]) -> Vec<&'a str> {
        v.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn user_text_is_working() {
        let lines = [user_text("hi")];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn assistant_tool_use_is_working() {
        let lines = [assistant_tool_use()];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn user_tool_result_is_working() {
        let lines = [user_tool_result()];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn assistant_text_only_is_done() {
        let lines = [assistant_text("here you go")];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Done));
    }

    #[test]
    fn metadata_after_text_does_not_override() {
        let lines = [
            assistant_text("hi"),
            meta("permission-mode"),
            meta("last-prompt"),
        ];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Done));
    }

    #[test]
    fn malformed_json_lines_are_skipped() {
        let lines = [assistant_tool_use(), "{ not json }".to_string()];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn empty_assistant_text_does_not_register() {
        let empty_assistant = json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "   " }] }
        })
        .to_string();
        let lines = [empty_assistant];
        let r = infer_state(&refs(&lines));
        // Saw a conversational entry (with content), so returns Some, but state is None.
        assert_eq!(r.unwrap().state, None);
    }

    #[test]
    fn only_metadata_returns_none() {
        let lines = [meta("permission-mode"), meta("last-prompt")];
        assert!(infer_state(&refs(&lines)).is_none());
    }

    #[test]
    fn extracts_model_and_summed_tokens() {
        let lines = [assistant_with_usage("claude-opus-4-7", 100, 2000, 40_000)];
        let r = infer_state(&refs(&lines)).unwrap();
        assert_eq!(r.state, Some(Status::Done));
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.input_tokens, Some(42_100));
    }

    #[test]
    fn state_newest_model_tokens_from_older_assistant() {
        let lines = [
            assistant_with_usage("claude-opus-4-7", 10, 0, 500),
            user_text("follow-up"),
        ];
        let r = infer_state(&refs(&lines)).unwrap();
        assert_eq!(r.state, Some(Status::Working));
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.input_tokens, Some(510));
    }

    #[test]
    fn synthetic_assistant_entry_is_ignored_for_model() {
        let synthetic = json!({
            "type": "assistant",
            "isSidechain": false,
            "message": {
                "role": "assistant",
                "model": "<synthetic>",
                "content": [{ "type": "text", "text": "api error" }],
                "usage": { "input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 }
            }
        })
        .to_string();
        let main = assistant_with_usage("claude-opus-4-7", 100, 2000, 40_000);
        let lines = [main, synthetic];
        let r = infer_state(&refs(&lines)).unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.input_tokens, Some(42_100));
    }

    #[test]
    fn sidechain_assistant_entry_is_ignored() {
        let sidechain = json!({
            "type": "assistant",
            "isSidechain": true,
            "message": {
                "role": "assistant",
                "model": "claude-haiku-4-5",
                "content": [{ "type": "text", "text": "sub-agent answer" }],
                "usage": { "input_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 500 }
            }
        })
        .to_string();
        let main = assistant_with_usage("claude-opus-4-7", 100, 2000, 40_000);
        let lines = [main, sidechain];
        let r = infer_state(&refs(&lines)).unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.input_tokens, Some(42_100));
    }

    #[test]
    fn past_assistant_plus_new_user_is_working() {
        let lines = [assistant_text("prev"), user_text("new")];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn tool_use_after_text_is_working() {
        let lines = [user_text("do X"), assistant_text("ok"), assistant_tool_use()];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn split_complete_partial_line_is_leftover() {
        let (lines, leftover) = split_complete("", "no newline yet");
        assert!(lines.is_empty());
        assert_eq!(leftover, "no newline yet");
    }

    #[test]
    fn split_complete_joins_leftover_with_next_chunk() {
        let (lines, leftover) = split_complete("par", "tial\ncomplete\n");
        assert_eq!(lines, vec!["partial", "complete"]);
        assert_eq!(leftover, "");
    }

    #[test]
    fn split_complete_trailing_line_stays_leftover() {
        let (lines, leftover) = split_complete("", "one\ntwo\npart");
        assert_eq!(lines, vec!["one", "two"]);
        assert_eq!(leftover, "part");
    }

    #[test]
    fn split_complete_drops_blank_lines() {
        let (lines, leftover) = split_complete("", "a\n\nb\n");
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(leftover, "");
    }

    // -------- apply_watcher_update tests --------

    fn make_session(status: Status) -> AgentSession {
        AgentSession {
            id: "s".into(),
            status,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            previous_prompts: Vec::new(),
            source: "claude-code".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
        }
    }

    #[test]
    fn merge_upgrades_done_to_working() {
        let mut s = make_session(Status::Done);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState { state: Some(Status::Working), ..Default::default() },
            1000,
        );
        assert!(changed);
        assert_eq!(s.status, Status::Working);
        assert_eq!(s.state_entered_at, 1000);
    }

    #[test]
    fn merge_does_not_downgrade_working_to_done() {
        let mut s = make_session(Status::Working);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState { state: Some(Status::Done), ..Default::default() },
            1000,
        );
        assert!(!changed);
        assert_eq!(s.status, Status::Working);
    }

    #[test]
    fn merge_does_not_override_awaiting() {
        let mut s = make_session(Status::Awaiting);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState { state: Some(Status::Done), ..Default::default() },
            1000,
        );
        assert!(!changed);
        assert_eq!(s.status, Status::Awaiting);
    }

    #[test]
    fn merge_error_to_working_is_allowed() {
        let mut s = make_session(Status::Error);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState { state: Some(Status::Working), ..Default::default() },
            1000,
        );
        assert!(changed);
        assert_eq!(s.status, Status::Working);
    }

    #[test]
    fn merge_updates_model_and_tokens_even_when_state_unchanged() {
        let mut s = make_session(Status::Working);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState {
                state: Some(Status::Working),
                model: Some("claude-opus-4-7".into()),
                input_tokens: Some(42_100),
            },
            500,
        );
        assert!(changed);
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.input_tokens, Some(42_100));
    }

    #[test]
    fn merge_noop_when_nothing_changes() {
        let mut s = make_session(Status::Working);
        s.model = Some("claude-opus-4-7".into());
        s.input_tokens = Some(100);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState {
                state: Some(Status::Working),
                model: Some("claude-opus-4-7".into()),
                input_tokens: Some(100),
            },
            500,
        );
        assert!(!changed);
    }
}
