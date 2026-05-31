//! Claude Code event adapter.
//!
//! Owns the decision logic for Claude Code lifecycle events: event-to-status
//! mapping, chat-id derivation from cwd / projects_root, prompt cleaning, and
//! transcript question-detection. The `integrations/claude_hook.py` shim is a
//! pure transport layer — it hands payloads to this module via `/api/event`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::adapters::AdapterOutput;
use crate::config::Config;
use crate::state::{DialogRole, PendingDialogEntry, SetInput, Status};

/// Built-in Claude Code tools that pause the model on a user decision. The
/// model's `tool_use` block isn't flushed to the JSONL transcript until the
/// user responds, so the watcher cannot see these calls in flight — the
/// PreToolUse hook is the only timely signal.
const USER_GATING_TOOLS: &[&str] = &["AskUserQuestion", "ExitPlanMode"];

/// Claude Code injects synthetic prompts (e.g. background-task completion
/// notices) as `<task-notification>` blocks. They are not real user input and
/// must not become dialog entries — filtered on both the hook path and the
/// transcript-watcher path.
pub(crate) fn is_system_injected(prompt: &str) -> bool {
    prompt.starts_with("<task-notification>")
}

fn awaiting_label_for(tool_name: &str) -> &'static str {
    match tool_name {
        "ExitPlanMode" => "plan approval",
        _ => "has a question",
    }
}

/// Translate a Claude Code hook event + payload into an [`AdapterOutput`].
///
/// Known events: `UserPromptSubmit`, `Stop`, `SessionStart`, `Notification`,
/// `PreToolUse`, `SessionEnd`. Unknown events → [`AdapterOutput::Ignore`].
/// `PostToolUse` is intentionally ignored — once the user answers, the
/// transcript watcher (and eventually `Stop`) carry the row out of `Awaiting`.
pub fn dispatch(event: &str, payload: &Value, cfg: &Config) -> AdapterOutput {
    let cwd = payload.get("cwd").and_then(|v| v.as_str());
    let projects_root = cfg.projects_root.as_deref();
    let chat_id = derive_chat_id(cwd, projects_root);

    if event == "SessionEnd" {
        return AdapterOutput::Clear { id: chat_id };
    }

    let Some((status, label)) = classify(event, payload, &cfg.benign_closers) else {
        return AdapterOutput::Ignore;
    };

    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    // Assistant text capture is owned by the transcript watcher
    // (`log_watcher::apply_and_emit` → `AppState::upsert_assistant_text`).
    // Stop's hook payload reaches the widget before Claude Code flushes the
    // final assistant turn to disk, so reading the transcript here would
    // record the previous turn's text. The watcher catches the post-Stop
    // write via `notify` and upserts the latest text instead.
    //
    // For UserPromptSubmit, take the *raw* prompt — not the cleaned `label`
    // — so the history window preserves newlines and the user's original
    // formatting. `clean_prompt` strips newlines for the one-line dashboard
    // row preview, which is the wrong shape for the multi-line history view.
    let dialog_entry = match event {
        "UserPromptSubmit" => payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| !is_system_injected(s))
            .map(|text| PendingDialogEntry {
                role: DialogRole::User,
                text: text.to_string(),
            }),
        _ => None,
    };

    AdapterOutput::Set {
        input: SetInput {
            id: chat_id,
            status,
            label,
            source: Some("claude".into()),
            model: None,
            input_tokens: None,
            dialog_entry,
        },
        transcript_path,
    }
}

/// Derive a friendly `chat_id` from `cwd` relative to `projects_root`.
///
/// - cwd under projects_root → relative path with `/`, `-`, `_` replaced by spaces
/// - cwd outside projects_root or no projects_root → basename of cwd
/// - no cwd → `claude-unknown` (defensive; Claude Code payloads always carry `cwd`)
fn derive_chat_id(cwd: Option<&str>, projects_root: Option<&str>) -> String {
    if let Some(cwd) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
        let normalized = cwd.replace('\\', "/");
        let normalized = normalized.trim_end_matches('/');
        if let Some(root) = projects_root.map(str::trim).filter(|s| !s.is_empty()) {
            let root = root.replace('\\', "/");
            let root = root.trim_end_matches('/');
            let prefix = format!("{}/", root);
            if normalized
                .to_lowercase()
                .starts_with(&prefix.to_lowercase())
            {
                let rel = &normalized[prefix.len()..];
                if !rel.is_empty() {
                    return rel
                        .chars()
                        .map(|c| match c {
                            '/' | '-' | '_' => ' ',
                            other => other,
                        })
                        .collect();
                }
            }
        }
        let basename = normalized.rsplit('/').next().unwrap_or("");
        if !basename.is_empty() {
            return basename.to_string();
        }
        return normalized.chars().take(20).collect();
    }
    "claude-unknown".to_string()
}

/// Map event + payload to a (status, optional label) pair.
///
/// Returns [`None`] for events we don't recognize (caller should `Ignore`).
/// Missing/empty `label` in the return tuple means "preserve prior label" in
/// the state layer.
fn classify(
    event: &str,
    payload: &Value,
    benign_closers: &[String],
) -> Option<(Status, Option<String>)> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());

    match event {
        "UserPromptSubmit" => {
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.trim().is_empty() {
                Some((Status::Working, None))
            } else {
                Some((Status::Working, Some(clean_prompt(prompt))))
            }
        }
        "Stop" => {
            if let Some(path) = transcript_path {
                if let Some(text) = last_assistant_text(Path::new(path)) {
                    if is_a_question(&text, benign_closers) {
                        return Some((Status::Awaiting, Some("has a question".into())));
                    }
                }
            }
            Some((Status::Done, None))
        }
        "PreToolUse" => {
            let tool_name = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            if !USER_GATING_TOOLS.contains(&tool_name) {
                return None;
            }
            Some((Status::Awaiting, Some(awaiting_label_for(tool_name).into())))
        }
        "Notification" | "SessionStart" => {
            let notif_type = payload
                .get("notification_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let message = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");

            if notif_type.is_empty() && message.trim().is_empty() {
                return Some((Status::Idle, None));
            }
            if notif_type == "idle_prompt" {
                if let Some(path) = transcript_path {
                    if let Some(text) = last_assistant_text(Path::new(path)) {
                        if is_a_question(&text, benign_closers) {
                            return Some((Status::Awaiting, Some("has a question".into())));
                        }
                    }
                }
                return Some((Status::Done, None));
            }
            let label = notification_label(notif_type, message);
            let cleaned = clean_prompt(&label);
            if cleaned.is_empty() {
                Some((Status::Awaiting, None))
            } else {
                // chars — not bytes — so multi-byte glyphs don't split mid-codepoint
                let truncated: String = cleaned.chars().take(60).collect();
                Some((Status::Awaiting, Some(truncated)))
            }
        }
        _ => None,
    }
}

fn notification_label(notif_type: &str, message: &str) -> String {
    match notif_type {
        "permission_prompt" => {
            let tool = if message.contains("use ") {
                message.rsplit_once("use ").map(|(_, t)| t).unwrap_or("tool")
            } else {
                "tool"
            };
            format!("needs approval: {}", tool)
        }
        "plan_approval" => "plan approval".into(),
        _ => message.to_string(),
    }
}

/// Normalize whitespace and strip Claude Code's terminal chrome (box-drawing,
/// block elements, misc technical) so labels read cleanly in the widget.
fn clean_prompt(text: &str) -> String {
    let stripped: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' | '\u{000B}' | '\u{000C}' => ' ',
            c if (0x2300..0x2400).contains(&(c as u32)) => ' ',
            c if (0x2500..0x25A0).contains(&(c as u32)) => ' ',
            c => c,
        })
        .collect();

    let mut collapsed = String::with_capacity(stripped.len());
    let mut prev_space = false;
    for c in stripped.chars() {
        if c == ' ' {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// Walk the transcript JSONL and return the latest non-empty assistant text
/// block. Returns `None` when the file is missing or contains no assistant
/// content.
fn last_assistant_text(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut last_text = String::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(msg) = value.get("message").filter(|v| v.is_object()) else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        match msg.get("content") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    last_text = trimmed.to_string();
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            last_text = trimmed.to_string();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if last_text.is_empty() { None } else { Some(last_text) }
}

/// Phrases that signal the agent has handed control back to the user — asking
/// permission, or asking a direct second-person question. Empirically derived
/// from real assistant messages — only patterns that actually appeared are
/// included; new ones are added as they're observed. Checked case-insensitively
/// in the last paragraph. A phrase ending in `?` matches literally; the rest
/// only count when a `?` follows them later in the paragraph.
/// `save this?` / `save these?` catch the `/reflect` and `/commit` save prompts,
/// whose menu can be trailed by a clause (`"… — then I'll run /commit."`) that
/// defeats the trailing-`?` path; the baked-in `?` keeps a declarative
/// `"save this config"` from matching. `can you` / `could you` / `did you` /
/// `want to` catch directed questions whose paragraph continues past the `?`
/// (`"Did you try the admin launch? That's the most likely fix."`).
const PERMISSION_SEEKING: &[&str] = &[
    "want me to",
    "shall i",
    "should i",
    "do you want",
    "save this?",
    "save these?",
    "can you",
    "could you",
    "did you",
    "want to",
];

/// True when `text` reads as a hand-back to the user: the whole text ends with
/// `?` (possibly after a trailing option list); the last paragraph contains a
/// known hand-back phrase (permission-seeking or a direct second-person
/// question); or the last paragraph issues a `Paste …` request for output.
fn is_a_question(text: &str, benign_closers: &[String]) -> bool {
    let plain = strip_markdown(text);
    let effective = strip_trailing_options(&plain);
    if effective.ends_with('?') {
        let lower = effective.to_lowercase();
        let is_benign = benign_closers.iter().any(|c| {
            !c.is_empty() && lower.ends_with(&c.to_lowercase())
        });
        if !is_benign {
            return true;
        }
    }
    has_permission_seeking_question(&plain) || has_paste_request(&plain)
}

/// Strip inline Markdown formatting so classification sees the underlying text.
/// Emphasis, code-span, heading and strikethrough markers are dropped — so a
/// final `**Push?**` reduces to `Push?` and is recognized as a question. Only
/// formatting characters are removed: newlines (paragraph structure) and every
/// other character — crucially the terminal `?` — are preserved.
fn strip_markdown(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '#' | '~'))
        .collect()
}

/// Check the last paragraph for a permission-seeking phrase. A phrase ending in
/// `?` matches as-is; the rest must be followed by a `?` later in the paragraph.
/// Catches questions embedded mid-paragraph like "Want me to add that? The
/// plan: ..." where the response continues after the question.
fn has_permission_seeking_question(text: &str) -> bool {
    let last_para = last_paragraph(text);
    let lower = last_para.to_lowercase();
    PERMISSION_SEEKING.iter().any(|phrase| {
        if let Some(phrase_start) = lower.find(phrase) {
            let after_phrase = &lower[phrase_start + phrase.len()..];
            phrase.ends_with('?') || after_phrase.contains('?')
        } else {
            false
        }
    })
}

/// True when the last paragraph issues a sentence-initial `Paste …` request —
/// the agent is waiting for the user to paste output back. Only a
/// sentence-initial imperative counts; a mid-sentence mention like
/// "you can paste this" or "I'll paste the result" does not.
fn has_paste_request(text: &str) -> bool {
    last_paragraph(text)
        .to_lowercase()
        .split(|c| matches!(c, '.' | '!' | '?' | '\n'))
        .any(|sentence| sentence.trim_start().starts_with("paste "))
}

fn last_paragraph(text: &str) -> &str {
    text.rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("")
}

/// Strip one trailing `(...)` group when it sits immediately after a `?`,
/// so `"Save these? (all / numbers / none)"` reduces to `"Save these?"`.
/// Returns the trimmed input unchanged when there's no such pattern.
fn strip_trailing_options(text: &str) -> &str {
    let trimmed = text.trim_end();
    if trimmed.ends_with(')') {
        if let Some(open_idx) = trimmed.rfind('(') {
            let before = trimmed[..open_idx].trim_end();
            if before.ends_with('?') {
                return before;
            }
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn cfg_with(projects_root: Option<&str>, benign_closers: &[&str]) -> Config {
        let mut cfg = Config::default();
        cfg.projects_root = projects_root.map(str::to_string);
        cfg.benign_closers = benign_closers.iter().map(|s| s.to_string()).collect();
        cfg
    }

    fn write_transcript(lines: &[Value]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude_code_dashboard_claude_adapter_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for v in lines {
            writeln!(f, "{}", serde_json::to_string(v).unwrap()).unwrap();
        }
        path
    }

    fn assistant_text(text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}]
            }
        })
    }

    // ----- derive_chat_id -----

    #[test]
    fn subfolder_of_projects_root_uses_spaced_relpath() {
        assert_eq!(
            derive_chat_id(Some("D:/projects/bga/assistant"), Some("d:/projects")),
            "bga assistant"
        );
    }

    #[test]
    fn dashes_and_underscores_become_spaces() {
        assert_eq!(
            derive_chat_id(Some("d:/projects/foo-bar/sub_dir/leaf"), Some("d:/projects")),
            "foo bar sub dir leaf"
        );
    }

    #[test]
    fn root_match_is_case_insensitive() {
        assert_eq!(
            derive_chat_id(Some("D:/PROJECTS/thing"), Some("d:/projects")),
            "thing"
        );
    }

    #[test]
    fn backslash_separators_are_normalized() {
        assert_eq!(
            derive_chat_id(Some("D:\\projects\\sub\\deep"), Some("d:/projects")),
            "sub deep"
        );
    }

    #[test]
    fn trailing_slash_on_cwd_is_tolerated() {
        assert_eq!(
            derive_chat_id(Some("d:/projects/foo-bar/"), Some("d:/projects")),
            "foo bar"
        );
    }

    #[test]
    fn exact_root_falls_back_to_basename() {
        assert_eq!(
            derive_chat_id(Some("d:/projects"), Some("d:/projects")),
            "projects"
        );
    }

    #[test]
    fn outside_projects_root_uses_basename() {
        assert_eq!(
            derive_chat_id(Some("c:/Users/foo/bar"), Some("d:/projects")),
            "bar"
        );
    }

    #[test]
    fn no_projects_root_uses_basename() {
        assert_eq!(
            derive_chat_id(Some("d:/projects/sub/deep"), None),
            "deep"
        );
    }

    #[test]
    fn no_cwd_returns_unknown() {
        assert_eq!(derive_chat_id(None, Some("d:/projects")), "claude-unknown");
    }

    #[test]
    fn whitespace_only_cwd_treated_as_missing() {
        assert_eq!(
            derive_chat_id(Some("   "), Some("d:/projects")),
            "claude-unknown"
        );
    }

    // ----- clean_prompt -----

    #[test]
    fn clean_prompt_flattens_multiline_with_single_spaces() {
        assert_eq!(
            clean_prompt("first line\nsecond  line\twith\ttabs"),
            "first line second line with tabs"
        );
    }

    #[test]
    fn clean_prompt_strips_terminal_chrome_glyphs() {
        assert_eq!(
            clean_prompt("⎿ Error: │ failed ▌ retry"),
            "Error: failed retry"
        );
    }

    #[test]
    fn clean_prompt_preserves_legitimate_unicode() {
        assert_eq!(clean_prompt("café 日本語 🚀 fix"), "café 日本語 🚀 fix");
    }

    #[test]
    fn clean_prompt_preserves_length_when_nothing_to_strip() {
        let prompt = "x".repeat(200);
        assert_eq!(clean_prompt(&prompt).len(), 200);
    }

    #[test]
    fn clean_prompt_empty_string() {
        assert_eq!(clean_prompt(""), "");
    }

    #[test]
    fn clean_prompt_whitespace_only() {
        assert_eq!(clean_prompt("   \t\n   "), "");
    }

    // ----- classify: UserPromptSubmit -----

    #[test]
    fn user_prompt_submit_with_prompt_returns_working_with_cleaned_label() {
        let p = json!({"prompt": "fix the bug"});
        let (status, label) = classify("UserPromptSubmit", &p, &[]).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn user_prompt_submit_with_blank_prompt_returns_working_without_label() {
        let p = json!({"prompt": "   "});
        let (status, label) = classify("UserPromptSubmit", &p, &[]).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label, None);
    }

    #[test]
    fn user_prompt_submit_missing_prompt_returns_working_without_label() {
        let p = json!({});
        let (status, label) = classify("UserPromptSubmit", &p, &[]).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label, None);
    }

    // ----- classify: Stop -----

    #[test]
    fn stop_without_transcript_is_done() {
        let (status, label) = classify("Stop", &json!({}), &[]).unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(label, None);
    }

    #[test]
    fn stop_with_question_ending_is_awaiting() {
        let t = write_transcript(&[assistant_text("Should I proceed?")]);
        let p = json!({"transcript_path": t.to_string_lossy()});
        let (status, label) = classify("Stop", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("has a question"));
        let _ = std::fs::remove_dir_all(t.parent().unwrap());
    }

    #[test]
    fn stop_without_question_ending_is_done() {
        let t = write_transcript(&[assistant_text("All tests passing.")]);
        let p = json!({"transcript_path": t.to_string_lossy()});
        let (status, label) = classify("Stop", &p, &[]).unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(label, None);
        let _ = std::fs::remove_dir_all(t.parent().unwrap());
    }

    // ----- classify: Notification -----

    #[test]
    fn notification_permission_prompt_extracts_tool() {
        let p = json!({
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"
        });
        let (status, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("needs approval: Bash"));
    }

    #[test]
    fn notification_plan_approval_fixed_label() {
        let p = json!({"notification_type": "plan_approval", "message": "ignored"});
        let (_, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(label.as_deref(), Some("plan approval"));
    }

    #[test]
    fn notification_idle_prompt_with_question_is_awaiting() {
        let t = write_transcript(&[assistant_text("What would you like me to do next?")]);
        let p = json!({
            "notification_type": "idle_prompt",
            "transcript_path": t.to_string_lossy(),
        });
        let (status, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("has a question"));
        let _ = std::fs::remove_dir_all(t.parent().unwrap());
    }

    #[test]
    fn notification_idle_prompt_without_question_is_done() {
        let t = write_transcript(&[assistant_text("All set.")]);
        let p = json!({
            "notification_type": "idle_prompt",
            "transcript_path": t.to_string_lossy(),
        });
        let (status, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(label, None);
        let _ = std::fs::remove_dir_all(t.parent().unwrap());
    }

    #[test]
    fn notification_without_type_but_with_message_is_awaiting() {
        let p = json!({"message": "Claude needs your attention"});
        let (status, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("Claude needs your attention"));
    }

    #[test]
    fn notification_label_truncates_to_60_chars() {
        let p = json!({"notification_type": "attention", "message": "y".repeat(200)});
        let (_, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(label.unwrap().chars().count(), 60);
    }

    #[test]
    fn notification_message_strips_terminal_chrome() {
        let p = json!({"message": "⎿  Error: pattern blocked"});
        let (_, label) = classify("Notification", &p, &[]).unwrap();
        assert_eq!(label.as_deref(), Some("Error: pattern blocked"));
    }

    // ----- classify: PreToolUse -----

    #[test]
    fn pre_tool_use_ask_user_question_is_awaiting_with_question_label() {
        let p = json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "?"}]}});
        let (status, label) = classify("PreToolUse", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("has a question"));
    }

    #[test]
    fn pre_tool_use_exit_plan_mode_is_awaiting_with_plan_label() {
        let p = json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "..."}});
        let (status, label) = classify("PreToolUse", &p, &[]).unwrap();
        assert_eq!(status, Status::Awaiting);
        assert_eq!(label.as_deref(), Some("plan approval"));
    }

    #[test]
    fn pre_tool_use_for_unrelated_tool_is_ignored() {
        let p = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        assert!(classify("PreToolUse", &p, &[]).is_none());
    }

    #[test]
    fn pre_tool_use_without_tool_name_is_ignored() {
        assert!(classify("PreToolUse", &json!({}), &[]).is_none());
    }

    // ----- classify: SessionStart -----

    #[test]
    fn session_start_with_no_fields_is_idle() {
        let (status, label) = classify("SessionStart", &json!({}), &[]).unwrap();
        assert_eq!(status, Status::Idle);
        assert_eq!(label, None);
    }

    // ----- classify: unknown -----

    #[test]
    fn unknown_event_returns_none() {
        assert!(classify("PreToolUse", &json!({}), &[]).is_none());
    }

    // ----- last_assistant_text -----

    #[test]
    fn missing_file_returns_none() {
        assert!(last_assistant_text(Path::new("/definitely/missing/transcript.jsonl")).is_none());
    }

    #[test]
    fn last_assistant_text_skips_user_entries() {
        let path = write_transcript(&[
            assistant_text("First answer?"),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "follow"}]}}),
            assistant_text("Ok, done."),
        ]);
        assert_eq!(last_assistant_text(&path).as_deref(), Some("Ok, done."));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn last_assistant_text_ignores_empty_blocks() {
        let path = write_transcript(&[assistant_text("Real question?"), assistant_text("   ")]);
        assert_eq!(last_assistant_text(&path).as_deref(), Some("Real question?"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn last_assistant_text_skips_malformed_lines() {
        let dir = std::env::temp_dir().join(format!(
            "claude_code_dashboard_claude_adapter_malformed_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "not json").unwrap();
        writeln!(
            f,
            "{}",
            serde_json::to_string(&assistant_text("Proceed?")).unwrap()
        )
        .unwrap();
        drop(f);
        assert_eq!(last_assistant_text(&path).as_deref(), Some("Proceed?"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- is_a_question -----

    #[test]
    fn is_a_question_simple_question_mark() {
        assert!(is_a_question("Should I proceed?", &[]));
    }

    #[test]
    fn is_a_question_no_question_mark() {
        assert!(!is_a_question("All done.", &[]));
    }

    #[test]
    fn is_a_question_empty_text() {
        assert!(!is_a_question("", &[]));
    }

    #[test]
    fn is_a_question_strips_trailing_option_list() {
        assert!(is_a_question(
            "Save these? (all / numbers / none)",
            &[]
        ));
    }

    #[test]
    fn is_a_question_bold_wrapped_question_is_detected() {
        // Regression: a final "**Push?**" must read as a question. The trailing
        // "**" used to hide the "?" so the row went Done instead of Awaiting.
        assert!(is_a_question(
            "Remote was in sync, so this will fast-forward cleanly.\n\n**Push?**",
            &[]
        ));
    }

    #[test]
    fn is_a_question_other_markdown_emphasis_is_stripped() {
        assert!(is_a_question("*continue?*", &[]));
        assert!(is_a_question("`run this?`", &[]));
        assert!(is_a_question("__ready to deploy?__", &[]));
        assert!(is_a_question("## Proceed?", &[]));
    }

    #[test]
    fn is_a_question_bold_question_with_trailing_options() {
        assert!(is_a_question("**Save these?** (all / none)", &[]));
    }

    #[test]
    fn is_a_question_bold_statement_is_not_a_question() {
        assert!(!is_a_question("**All done.**", &[]));
    }

    #[test]
    fn is_a_question_strips_trailing_option_list_with_extra_whitespace() {
        assert!(is_a_question(
            "Continue?   (yes / no)  \n",
            &[]
        ));
    }

    #[test]
    fn is_a_question_does_not_strip_unrelated_parens() {
        // Trailing "(foo.py)" doesn't follow a "?", so we don't strip and the
        // text doesn't end with "?" → not a question.
        assert!(!is_a_question("Look at this code (foo.py)", &[]));
    }

    #[test]
    fn is_a_question_keeps_inline_parens() {
        assert!(is_a_question("Should I update foo (the helper)?", &[]));
    }

    #[test]
    fn is_a_question_benign_closer_with_options_is_not_a_question() {
        let closers = vec!["What's next?".to_string()];
        assert!(!is_a_question("What's next? (continue / stop)", &closers));
    }

    #[test]
    fn is_a_question_benign_closer_alone_is_not_a_question() {
        let closers = vec!["What's next?".to_string()];
        for text in ["What's next?", "what's next?", "Done. What's next?"] {
            assert!(!is_a_question(text, &closers), "text: {}", text);
        }
    }

    #[test]
    fn is_a_question_non_matching_closer_still_awaits() {
        let closers = vec!["What's next?".to_string()];
        assert!(is_a_question("Which option do you prefer?", &closers));
    }

    // ----- permission-seeking in last paragraph -----

    #[test]
    fn permission_seeking_want_me_to_mid_paragraph() {
        assert!(is_a_question(
            "The state is ephemeral. Want me to add persistence? The plan: write sessions.json to disk.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_shall_i_mid_paragraph() {
        assert!(is_a_question(
            "Three changes here. Shall I proceed? I'll create separate commits.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_should_i_mid_paragraph() {
        assert!(is_a_question(
            "Found the issue. Should I use the cached value? It would avoid the network call.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_do_you_want_mid_paragraph() {
        assert!(is_a_question(
            "Deployed. Do you want me to run the tests? I can also check coverage.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_save_prompt_with_trailing_clause() {
        // The /reflect and /commit skills close with "Save this/these? (...)"
        // menus a trailing clause can follow, so the text doesn't end with "?".
        // The "save this?"/"save these?" phrasing flips it to awaiting.
        assert!(is_a_question(
            "Save this? (all / 1 / none) — then I'll run /commit.",
            &[]
        ));
        assert!(is_a_question(
            "Save these? (all / numbers / none) — I'll commit after.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_save_without_question_mark_is_not_awaiting() {
        // The "?" is baked into the phrase, so a declarative "save this ..."
        // must not match — only the literal "save this?" question does.
        assert!(!is_a_question(
            "Let me save this config before continuing. Running the build now.",
            &[]
        ));
    }

    #[test]
    fn directed_question_mid_paragraph_is_awaiting() {
        // Direct second-person questions whose paragraph continues past the "?".
        for text in [
            "Can you reopen the history and confirm the line is there? If not, I'll dig in.",
            "Could you paste that line? It'll look like a JSON blob.",
            "Did you try the admin launch? That's the most likely fix.",
            "Want to try a clean test? Reset the config and relaunch.",
        ] {
            assert!(is_a_question(text, &[]), "text: {}", text);
        }
    }

    #[test]
    fn paste_request_is_awaiting() {
        // A sentence-initial "Paste ..." imperative means the agent is waiting.
        assert!(is_a_question("Paste the tableinfos output and I'll finish arena.", &[]));
        assert!(is_a_question("Looks good. Paste whatever it prints.", &[]));
    }

    #[test]
    fn paste_mention_mid_sentence_is_not_awaiting() {
        // Only a sentence-initial imperative counts — a mention does not.
        assert!(!is_a_question("You can paste this into the terminal later. All set.", &[]));
        assert!(!is_a_question("I'll paste the result here once it's done.", &[]));
    }

    #[test]
    fn permission_seeking_only_checks_last_paragraph() {
        // Question in first paragraph, statement in last — should NOT match.
        assert!(!is_a_question(
            "Want me to fix it?\n\nI went ahead and fixed it. All tests pass.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_case_insensitive() {
        assert!(is_a_question(
            "WANT ME TO add this? Here's the plan.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_no_question_mark_after_phrase() {
        assert!(!is_a_question(
            "I want me to clarify: the fix is in place. All done.",
            &[]
        ));
    }

    #[test]
    fn permission_seeking_does_not_match_unrelated_question_in_last_para() {
        // "?" exists in last paragraph but no permission-seeking phrase.
        // The text also doesn't end with "?" so neither check fires.
        assert!(!is_a_question(
            "Let me investigate — does the bug have a cleaner fix at the rotate_vector level? This affects what we do next.",
            &[]
        ));
    }

    // ----- dispatch: integration -----

    #[test]
    fn dispatch_session_end_returns_clear() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({"cwd": "d:/projects/foo"});
        match dispatch("SessionEnd", &p, &cfg) {
            AdapterOutput::Clear { id } => assert_eq!(id, "foo"),
            _ => panic!("expected Clear"),
        }
    }

    #[test]
    fn dispatch_unknown_event_is_ignore() {
        let cfg = cfg_with(None, &[]);
        assert!(matches!(
            dispatch("PreToolUse", &json!({}), &cfg),
            AdapterOutput::Ignore
        ));
    }

    #[test]
    fn dispatch_user_prompt_submit_produces_set_with_transcript() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({
            "cwd": "d:/projects/demo",
            "session_id": "s",
            "prompt": "fix bug",
            "transcript_path": "/tmp/t.jsonl"
        });
        match dispatch("UserPromptSubmit", &p, &cfg) {
            AdapterOutput::Set { input, transcript_path } => {
                assert_eq!(input.id, "demo");
                assert_eq!(input.status, Status::Working);
                assert_eq!(input.label.as_deref(), Some("fix bug"));
                assert_eq!(input.source.as_deref(), Some("claude"));
                assert_eq!(transcript_path.as_deref(), Some(Path::new("/tmp/t.jsonl")));
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_user_prompt_preserves_newlines_in_dialog_entry() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({
            "cwd": "d:/projects/demo",
            "session_id": "s",
            "prompt": "line one\n\nline two\nline three",
        });
        match dispatch("UserPromptSubmit", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                // Label is collapsed for the one-line dashboard preview
                assert_eq!(input.label.as_deref(), Some("line one line two line three"));
                // Dialog entry keeps the raw prompt so the history view can format it
                let entry = input.dialog_entry.expect("dialog_entry present");
                assert_eq!(entry.role, DialogRole::User);
                assert_eq!(entry.text, "line one\n\nline two\nline three");
            }
            _ => panic!("expected Set"),
        }
    }
}
