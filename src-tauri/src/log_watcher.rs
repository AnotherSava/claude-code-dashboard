use crate::commands::{emit_sessions_updated, now_ms};
use crate::config::ConfigState;
use crate::prompt_history::PromptHistoryStore;
use crate::state::{AgentSession, AppState, Status};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

/// Claude Code writes this as a `user` transcript entry when a turn is
/// cancelled with Esc (it has variants like "… for tool use", so we match the
/// prefix). An Esc-cancel fires no lifecycle hook, so this marker is the only
/// signal that the turn ended — `infer_state` flags it so the watcher can
/// demote the row out of `Working` instead of re-reading it as user input.
const INTERRUPT_MARKER_PREFIX: &str = "[Request interrupted by user";

/// Block-level output of one inference pass over transcript lines. Fields are
/// `None` when the scan found nothing conclusive for that dimension — callers
/// are expected to preserve prior values rather than clobber to None.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InferredState {
    pub state: Option<Status>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    /// The newest state-bearing entry is an Esc-cancel interrupt marker — the
    /// turn ended with no hook. Drives a `Working`→`Idle` demotion.
    pub ended: bool,
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
        if entry.entry_type == "system" {
            continue;
        }
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

        // Sidechain (Task subagent / background-agent) entries run in their own
        // context and never determine the main row's state — skipping them keeps
        // background work from flipping a `Waiting` row (set at `Stop` time from
        // the hook's `background_tasks`) back to `Working`. Model/tokens are
        // already main-session-only above.
        if result.state.is_none() && !entry.is_sidechain {
            let has_tool_use = content.iter().any(|b| b.block_type == "tool_use");
            let has_tool_result = content.iter().any(|b| b.block_type == "tool_result");
            let has_text = content.iter().any(|b| {
                b.block_type == "text"
                    && b.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false)
            });
            if has_tool_use || has_tool_result {
                result.state = Some(Status::Working);
            } else if entry.entry_type == "user" && has_text {
                // An interrupt marker is the newest user entry only when the
                // turn was just cancelled (a fresh prompt afterwards would be
                // newer). Flag the end and stop here so an older entry can't
                // re-resolve the state to Working.
                if content.iter().any(|b| {
                    b.block_type == "text"
                        && b.text.as_deref().map(|t| t.trim().starts_with(INTERRUPT_MARKER_PREFIX)).unwrap_or(false)
                }) {
                    result.ended = true;
                    result.state = Some(Status::Idle); // sentinel: stop resolution; apply_watcher_update ignores non-Working
                } else {
                    result.state = Some(Status::Working);
                }
            } else if entry.entry_type == "assistant" && has_text {
                result.state = Some(Status::Done);
            }
        }

        if result.state.is_some() && result.model.is_some() && result.input_tokens.is_some() {
            break;
        }
    }

    if !saw_conversational
        && result.state.is_none()
        && result.model.is_none()
        && result.input_tokens.is_none()
    {
        return None;
    }
    Some(result)
}

use crate::state::DialogRole;

/// Walk lines forward (chronological) and extract text entries for the dialog.
/// Captures assistant text blocks and mid-turn queued/interrupt prompts
/// (`queued_command` attachments), which carry their text at
/// `attachment.prompt`. System-injected prompts (task notifications) are
/// skipped, mirroring the hook path's filter.
pub fn extract_text_entries(lines: &[&str]) -> Vec<(DialogRole, String)> {
    let mut entries = Vec::new();
    for line in lines {
        let entry: TranscriptEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.entry_type == "attachment" {
            let queued = entry
                .attachment
                .as_ref()
                .filter(|a| a.attachment_type.as_deref() == Some("queued_command"))
                .and_then(|a| a.prompt.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter(|s| !crate::adapters::claude::is_system_injected(s));
            if let Some(prompt) = queued {
                entries.push((DialogRole::User, prompt.to_string()));
            }
            continue;
        }
        if entry.entry_type == "assistant" && !entry.is_sidechain {
            let content = entry.message.as_ref().and_then(|m| m.content.as_ref());
            if let Some(blocks) = content {
                let mut latest: Option<String> = None;
                for b in blocks {
                    if b.block_type == "text" {
                        if let Some(ref t) = b.text {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                latest = Some(trimmed.to_string());
                            }
                        }
                    }
                }
                if let Some(text) = latest {
                    entries.push((DialogRole::Assistant, text));
                }
            }
        }
    }
    entries
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
/// blocked, error) — those are hook-authoritative. Returns true if anything
/// actually changed.
pub fn apply_watcher_update(
    session: &mut AgentSession,
    update: &InferredState,
    now_ms: i64,
) -> bool {
    let mut changed = false;
    // The watcher drives the two transcript-derived *active* states. `Working` is
    // a pure promote (carry a too-early `Stop` back). `Waiting` is the "main turn
    // done, background agents still running" state and may move a row off
    // `Working` — but only when the turn genuinely resolved Done with agents
    // pending (see `infer_state`), never on a stale read. Done/Idle/Blocked come
    // from lifecycle hooks, so the watcher leaves them alone.
    match update.state {
        Some(s @ (Status::Working | Status::Waiting)) if session.status != s => {
            session.status = s;
            session.state_entered_at = now_ms;
            changed = true;
        }
        _ => {}
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
    attachment: Option<TranscriptAttachment>,
}

/// The `attachment` payload on an `attachment`-type transcript entry. A
/// mid-turn queued/interrupt prompt arrives as `attachment.type ==
/// "queued_command"` with the text under `attachment.prompt` — there is no
/// top-level `prompt` field and no `UserPromptSubmit` hook for it, so the
/// transcript is the only place this user input appears.
#[derive(Deserialize)]
struct TranscriptAttachment {
    #[serde(rename = "type")]
    attachment_type: Option<String>,
    prompt: Option<String>,
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

    pub fn current_path(&self, chat_id: &str) -> Option<PathBuf> {
        self.entries.lock().unwrap().get(chat_id).map(|t| t.path.clone())
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

    ensure_watch_dir(&parent);

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

/// A brand-new project's transcript dir doesn't exist yet at SessionStart —
/// Claude Code creates it lazily when it writes the first turn. Watching a
/// missing dir fails permanently (no retry), which strands the session: the
/// watcher is the only thing that promotes Blocked -> Working mid-turn, so a
/// post-question resume never clears. Pre-create the dir (idempotent; it's the
/// exact dir Claude is about to use) so the watch always attaches.
fn ensure_watch_dir(dir: &Path) {
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "failed to pre-create transcript dir");
        }
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

    let text_entries = if initial_read {
        state.lock().unwrap().initial_read = false;
        update.state = None;
        // A stale interrupt marker at the tail of a pre-existing transcript
        // must not demote a session being restored on app start.
        update.ended = false;
        let all = extract_text_entries(&borrowed);
        all.into_iter().rev().find(|(r, _)| *r == DialogRole::Assistant).into_iter().collect()
    } else {
        extract_text_entries(&borrowed)
    };

    apply_and_emit(app, chat_id, &update, text_entries);
}

fn apply_and_emit(app: &AppHandle, chat_id: &str, update: &InferredState, text_entries: Vec<(DialogRole, String)>) {
    let Some(app_state) = app.try_state::<AppState>() else {
        return;
    };
    let now = now_ms();
    let (metric_changed, prior_status, new_status) = {
        let mut sessions = app_state.sessions.lock().unwrap();
        match sessions.iter_mut().find(|s| s.id == chat_id) {
            Some(session) => {
                let prior = session.status;
                let changed = apply_watcher_update(session, update, now);
                (changed, prior, session.status)
            }
            None => (false, Status::Idle, Status::Idle),
        }
    };
    if prior_status != Status::Working && new_status == Status::Working {
        // Carried a row back to Working without a lifecycle hook — e.g. the user
        // answered an AskUserQuestion and the agent resumed.
        tracing::debug!(
            chat_id,
            decision = "resume_working",
            reason = "transcript shows new activity (tool call or user turn) after a pause; promoted to Working",
            "decision"
        );
    }
    // The turn was cancelled with Esc (no lifecycle hook). Settle the row back
    // to its pre-prompt status — unless the user opted out. Gated here rather
    // than in `infer_state` so detection stays pure and testable.
    let reverted_to = if update.ended
        && app
            .try_state::<ConfigState>()
            .map(|c| c.snapshot().detect_cancelled_turns)
            .unwrap_or(true)
    {
        app_state.revert_cancelled_turn(chat_id, now)
    } else {
        None
    };
    if let Some(status) = reverted_to {
        tracing::debug!(
            chat_id,
            decision = "revert_cancelled",
            status = ?status,
            reason = "turn cancelled with Esc (interrupt marker, no lifecycle hook); reverted to pre-prompt status",
            "decision"
        );
    }
    let demoted = reverted_to.is_some();
    let dialog_changed = if !text_entries.is_empty() {
        app_state.apply_text_entries(chat_id, &text_entries, now)
    } else {
        false
    };
    if dialog_changed {
        if let Some(h) = app.try_state::<PromptHistoryStore>() {
            let sessions = app_state.sessions.lock().unwrap();
            if let Some(s) = sessions.iter().find(|s| s.id == chat_id) {
                h.save_session(s);
            }
            drop(sessions);
            h.save_to_disk();
        }
    }
    if metric_changed || dialog_changed || demoted {
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

    /// A `turn_duration` system record. Claude Code includes
    /// `pendingBackgroundAgentCount` only when background subagents are still
    /// running, so `None` omits the field (mirroring a zero-pending turn).
    fn turn_duration(pending: Option<u64>) -> String {
        let mut o = json!({ "type": "system", "subtype": "turn_duration", "isSidechain": false });
        if let Some(n) = pending {
            o["pendingBackgroundAgentCount"] = json!(n);
        }
        o.to_string()
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
    fn interrupt_marker_flags_ended_not_working() {
        // The newest entry is an Esc-cancel marker following a working turn.
        let lines = [
            user_text("do the thing"),
            assistant_tool_use(),
            user_text("[Request interrupted by user]"),
        ];
        let r = infer_state(&refs(&lines)).unwrap();
        assert!(r.ended, "interrupt marker sets ended");
        assert_ne!(r.state, Some(Status::Working), "must not re-promote to working");
    }

    #[test]
    fn interrupt_marker_variant_for_tool_use_flags_ended() {
        let lines = [user_text("[Request interrupted by user for tool use]")];
        assert!(infer_state(&refs(&lines)).unwrap().ended);
    }

    #[test]
    fn prompt_after_interrupt_is_working_again() {
        // A fresh prompt newer than the interrupt marker is a new turn.
        let lines = [
            user_text("[Request interrupted by user]"),
            user_text("ok now do this instead"),
        ];
        let r = infer_state(&refs(&lines)).unwrap();
        assert!(!r.ended, "the newer entry is a real prompt, not a cancel");
        assert_eq!(r.state, Some(Status::Working));
    }

    // -------- turn_duration / sidechain skipping --------
    // The `Waiting` state is now set at `Stop` time from the hook's
    // `background_tasks` payload (see `adapters::claude::classify_stop`), not
    // inferred from the transcript's `pendingBackgroundAgentCount`. `infer_state`
    // just skips `turn_duration` system records and sidechain entries so neither
    // can disturb the main row's state.

    #[test]
    fn active_turn_with_trailing_turn_duration_stays_working() {
        // Real tool use just now, with a trailing turn_duration system record →
        // the system record is skipped and the main turn stays Working.
        let lines = [assistant_tool_use(), turn_duration(Some(2))];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Working));
    }

    #[test]
    fn trailing_turn_duration_does_not_disturb_done() {
        // A turn_duration flushed after the final assistant text is a system
        // record → skipped, so the assistant-text Done verdict stands.
        let lines = [assistant_text("All batches in."), turn_duration(None)];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Done));
    }

    #[test]
    fn interrupt_marker_not_overridden_by_turn_duration() {
        // An Esc-cancel must still demote even with an older turn_duration present.
        let lines = [
            turn_duration(Some(2)),
            assistant_tool_use(),
            user_text("[Request interrupted by user]"),
        ];
        let r = infer_state(&refs(&lines)).unwrap();
        assert!(r.ended);
        assert_ne!(r.state, Some(Status::Working));
    }

    #[test]
    fn sidechain_activity_does_not_set_state() {
        // Background / Task subagent (sidechain) tool use after the main turn's
        // Done text must not flip the row to Working — the main Done text wins, so
        // a Waiting row (set at Stop from background_tasks) is left alone by the
        // watcher's promote-only update.
        let sidechain_tool = json!({
            "type": "assistant", "isSidechain": true,
            "message": { "role": "assistant", "content": [{ "type": "tool_use", "name": "Read" }] }
        })
        .to_string();
        let lines = [assistant_text("Batch B done."), sidechain_tool];
        assert_eq!(infer_state(&refs(&lines)).unwrap().state, Some(Status::Done));
    }

    // -------- extract_text_entries tests --------

    fn attachment(prompt: &str) -> String {
        json!({ "type": "attachment", "attachment": { "type": "queued_command", "prompt": prompt, "commandMode": "prompt" } }).to_string()
    }

    #[test]
    fn extract_captures_assistant_text_in_order() {
        let lines = [assistant_text("first"), assistant_tool_use(), assistant_text("second")];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (DialogRole::Assistant, "first".into()));
        assert_eq!(entries[1], (DialogRole::Assistant, "second".into()));
    }

    #[test]
    fn extract_skips_tool_only_entries() {
        let lines = [user_text("hi"), assistant_tool_use(), user_tool_result()];
        let entries = extract_text_entries(&refs(&lines));
        assert!(entries.is_empty());
    }

    #[test]
    fn extract_skips_sidechain() {
        let sidechain = json!({
            "type": "assistant",
            "isSidechain": true,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "sub-agent" }] }
        })
        .to_string();
        let lines = [assistant_text("main"), sidechain];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "main");
    }

    #[test]
    fn extract_takes_last_block_in_entry() {
        let multi = json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "first block" },
                    { "type": "tool_use", "name": "Read" },
                    { "type": "text", "text": "second block" }
                ]
            }
        })
        .to_string();
        let lines = [multi];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "second block");
    }

    #[test]
    fn extract_captures_attachment_as_user() {
        let lines = [assistant_text("working..."), attachment("check the build failure"), assistant_text("done")];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (DialogRole::Assistant, "working...".into()));
        assert_eq!(entries[1], (DialogRole::User, "check the build failure".into()));
        assert_eq!(entries[2], (DialogRole::Assistant, "done".into()));
    }

    #[test]
    fn extract_skips_empty_attachment() {
        let lines = [attachment("  "), assistant_text("answer")];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "answer");
    }

    #[test]
    fn extract_skips_task_notification_attachment() {
        // queued_command can also carry a system-injected task notification —
        // not real user input, must not become a dialog boundary.
        let sysmsg = json!({
            "type": "attachment",
            "attachment": {
                "type": "queued_command",
                "prompt": "<task-notification>\n<status>completed</status>\n</task-notification>",
                "commandMode": "task-notification"
            }
        })
        .to_string();
        let lines = [sysmsg, assistant_text("answer")];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "answer");
    }

    #[test]
    fn extract_skips_non_queued_attachment() {
        // Other attachment kinds (task reminders, edited-file notices, etc.)
        // have no user prompt and must be ignored.
        let reminder = json!({
            "type": "attachment",
            "attachment": { "type": "task_reminder", "content": [], "itemCount": 0 }
        })
        .to_string();
        let lines = [reminder, assistant_text("answer")];
        let entries = extract_text_entries(&refs(&lines));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "answer");
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
            status_before_working: Status::Idle,
            label: String::new(),
            original_prompt: None,
            task_started_at: 0,
            dialog: Vec::new(),
            source: "claude-code".into(),
            model: None,
            input_tokens: None,
            updated: 0,
            state_entered_at: 0,
            working_accumulated_ms: 0,
            display_name: None,
            origin: None,
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
    fn merge_does_not_override_blocked() {
        let mut s = make_session(Status::Blocked);
        let changed = apply_watcher_update(
            &mut s,
            &InferredState { state: Some(Status::Done), ..Default::default() },
            1000,
        );
        assert!(!changed);
        assert_eq!(s.status, Status::Blocked);
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
                ended: false,
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
                ended: false,
            },
            500,
        );
        assert!(!changed);
    }

    #[test]
    fn ensure_watch_dir_creates_missing_nested_dir() {
        // Regression: a brand-new project's transcript dir doesn't exist at
        // SessionStart, and watching a missing dir fails permanently — stranding
        // the session on Blocked. ensure_watch_dir must create it first.
        let base = std::env::temp_dir().join(format!(
            "ccd-watch-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("-Users-someone-Projects-players");
        assert!(!dir.exists());

        ensure_watch_dir(&dir);
        assert!(dir.exists() && dir.is_dir());

        // Idempotent: a second call on an existing dir is a no-op, not an error.
        ensure_watch_dir(&dir);
        assert!(dir.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
