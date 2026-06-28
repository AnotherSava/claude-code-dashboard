//! Claude Code event adapter.
//!
//! Owns the decision logic for Claude Code lifecycle events: event-to-status
//! mapping, chat-id derivation from cwd / projects_root, prompt cleaning, and
//! transcript question-detection. The `integrations/claude_hook.py` shim is a
//! pure transport layer — it hands payloads to this module via `/api/event`.

use std::path::PathBuf;

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

fn blocked_label_for(tool_name: &str) -> &'static str {
    match tool_name {
        "ExitPlanMode" => "plan approval",
        _ => "has a question",
    }
}

/// Translate a Claude Code hook event + payload into an [`AdapterOutput`].
///
/// Known events: `UserPromptSubmit`, `UserPromptExpansion`, `Stop`,
/// `StopFailure`, `SessionStart`, `Notification`, `PreToolUse`,
/// `PermissionRequest`, `Elicitation`, `ElicitationResult`, `PreCompact`,
/// `SessionEnd`. Unknown events → [`AdapterOutput::Ignore`].
/// `ElicitationResult` is the authoritative unblock for an `Elicitation` (user
/// answered the MCP prompt → resume `Working`). `UserPromptExpansion` is the early signal
/// a slash command fires before its context-gathering, so a skill launch shows
/// Working at once instead of after the gathering when `UserPromptSubmit` lands.
/// `StopFailure` (API error), `PermissionRequest`/`Elicitation` (blocked on the
/// user), and `PreCompact` (context boundary → history separator) fill gaps the
/// lifecycle events leave.
/// `PostToolUse` is intentionally ignored — once the user answers, the
/// transcript watcher (and eventually `Stop`) carry the row out of `Blocked`.
pub fn dispatch(event: &str, payload: &Value, cfg: &Config) -> AdapterOutput {
    let cwd = payload.get("cwd").and_then(|v| v.as_str());
    let projects_root = cfg.projects_root.as_deref();
    let chat_id = derive_chat_id(cwd, projects_root);

    if event == "SessionEnd" {
        return AdapterOutput::Clear { id: chat_id };
    }

    // Context compaction (manual `/compact` or auto): the session continues but
    // its prior dialog belongs to the pre-compaction context — drop a history
    // separator. Idempotent in the state layer, so it's safe even if a
    // transcript-path rotation also marks the same boundary.
    if event == "PreCompact" {
        return AdapterOutput::Boundary { id: chat_id };
    }

    let Some(Classification { status, label, reason }) = classify_detailed(event, payload, QuestionRules::from_config(cfg)) else {
        return AdapterOutput::Ignore;
    };

    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    // Assistant text capture is owned by the transcript watcher
    // (`log_watcher::apply_and_emit` → `AppState::apply_text_entries`).
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
        reason,
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
/// A classified event: the resulting status/label plus a human-readable
/// `reason` for the decision log. Built by [`classify_detailed`]; the public
/// [`classify`] drops `reason` for callers (tests) that only assert status/label.
struct Classification {
    status: Status,
    label: Option<String>,
    reason: String,
}

impl Classification {
    fn new(status: Status, label: Option<String>, reason: impl Into<String>) -> Self {
        Self { status, label, reason: reason.into() }
    }
}

/// Thin wrapper over [`classify_detailed`] returning just `(status, label)`.
/// Kept so the adapter's unit tests assert the user-visible outcome without
/// threading the decision-log `reason` through every assertion.
#[cfg(test)]
fn classify(
    event: &str,
    payload: &Value,
    rules: QuestionRules,
) -> Option<(Status, Option<String>)> {
    classify_detailed(event, payload, rules).map(|c| (c.status, c.label))
}

/// Classify the `Stop` hook straight from its payload — authoritative, no
/// transcript read. Claude Code carries the final assistant text as
/// `last_assistant_message` ("Text content of the last assistant message before
/// stopping. Avoids the need to read and parse the transcript file.") and the
/// in-flight background work as `background_tasks` (an array, empty/absent when
/// nothing is running). Precedence: a hand-back question → `Blocked`; else
/// background work still running → `Waiting` ("looks done but isn't"); else
/// `Done`. This replaces the old transcript-scan path and the after-the-fact
/// `Stop`-was-stale corrections, which existed only because the hook used to
/// fire before the final turn flushed to JSONL.
fn classify_stop(payload: &Value, rules: QuestionRules) -> Classification {
    let final_text = payload
        .get("last_assistant_message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(text) = final_text {
        if let Some(rule) = question_reason(text, rules) {
            return Classification::new(
                Status::Blocked,
                Some("has a question".into()),
                format!("turn ended on a question [{rule}]: \"{}\"", evidence_snippet(text)),
            );
        }
    }

    // `background_tasks` is present (and non-empty) only while background work is
    // in flight — the signal for the "main turn settled but subagents are still
    // running" Waiting state, set here at Stop time instead of scraped from the
    // transcript's `pendingBackgroundAgentCount`. Label is left unset so the
    // working task label is preserved while the row waits.
    let background_pending = payload
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .filter(|tasks| !tasks.is_empty());
    if let Some(tasks) = background_pending {
        return Classification::new(
            Status::Waiting,
            None,
            format!("turn ended with {} background task(s) still in flight → waiting", tasks.len()),
        );
    }

    match final_text {
        Some(text) => Classification::new(Status::Done, None, format!("turn ended; final message is not a question: \"{}\"", evidence_snippet(text))),
        None => Classification::new(Status::Done, None, "turn ended; no final assistant message in payload"),
    }
}

fn classify_detailed(
    event: &str,
    payload: &Value,
    rules: QuestionRules,
) -> Option<Classification> {
    match event {
        "UserPromptSubmit" => {
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.trim().is_empty() {
                Some(Classification::new(Status::Working, None, "user submitted a prompt (empty/continuation)"))
            } else {
                Some(Classification::new(Status::Working, Some(clean_prompt(prompt)), "user submitted a prompt"))
            }
        }
        "UserPromptExpansion" => {
            // Fires the instant a slash command is invoked — seconds before
            // `UserPromptSubmit`, which Claude Code only emits *after* the
            // command's `!` context-gathering finishes. Flip to Working now with
            // the command (e.g. "/commit") as the label so a skill launch
            // doesn't sit on the prior DONE/IDLE state for the gathering window;
            // the later `UserPromptSubmit` reaffirms the same task. Only handle
            // slash-command expansions — other expansion types are left for
            // `UserPromptSubmit`. No dialog entry here (UserPromptSubmit owns it).
            if payload.get("expansion_type").and_then(|v| v.as_str()) != Some("slash_command") {
                return None;
            }
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.trim().is_empty() {
                Some(Classification::new(Status::Working, None, "slash command invoked (early Working signal)"))
            } else {
                Some(Classification::new(Status::Working, Some(clean_prompt(prompt)), "slash command invoked (early Working signal)"))
            }
        }
        "Stop" => Some(classify_stop(payload, rules)),
        "PreToolUse" => {
            let tool_name = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            if !USER_GATING_TOOLS.contains(&tool_name) {
                return None;
            }
            Some(Classification::new(
                Status::Blocked,
                Some(blocked_label_for(tool_name).into()),
                format!("{tool_name} tool gated the turn on the user (buffered until answered)"),
            ))
        }
        "Notification" | "SessionStart" => {
            let notif_type = payload
                .get("notification_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let message = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");

            if notif_type.is_empty() && message.trim().is_empty() {
                return Some(Classification::new(Status::Idle, None, format!("{event} with no notification payload → idle")));
            }
            // `idle_prompt` (Claude sitting at the idle input prompt ~60s after a
            // turn) is ignored: `Stop` already settled the row authoritatively
            // from its payload, so re-deriving the verdict from the transcript
            // here is pure redundancy — and `idle_prompt` is a flaky fixed timer
            // we can't lean on (often doesn't fire, never for `AskUserQuestion`).
            // A blanket settle is also wrong: a pending plain-text question sits
            // at the same idle prompt and must stay `Blocked`.
            if notif_type == "idle_prompt" {
                return None;
            }
            let label = notification_label(notif_type, message);
            let cleaned = clean_prompt(&label);
            if cleaned.is_empty() {
                Some(Classification::new(Status::Blocked, None, format!("notification [{notif_type}] blocked on user")))
            } else {
                // chars — not bytes — so multi-byte glyphs don't split mid-codepoint
                let truncated: String = cleaned.chars().take(60).collect();
                Some(Classification::new(Status::Blocked, Some(truncated), format!("notification [{notif_type}] blocked on user")))
            }
        }
        "StopFailure" => {
            // The turn ended on an API error — rate limit, overload, billing,
            // auth, server error, max output tokens, … — which fires no `Stop`,
            // so without this the row would sit on WORK until something else
            // settles it. Surface ERROR with the kind. The exact payload field
            // is read defensively (confirmed empirically) with a generic
            // fallback so the state is always correct even if the kind is absent.
            let reason = ["reason", "error_type", "error", "type", "failure_reason", "message"]
                .iter()
                .find_map(|k| payload.get(k).and_then(|v| v.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(clean_prompt)
                .filter(|s| !s.is_empty());
            let kind = reason.unwrap_or_else(|| "turn failed".into());
            Some(Classification::new(Status::Error, Some(kind.clone()), format!("turn failed on API error: {kind}")))
        }
        "PermissionRequest" => {
            // A tool-permission dialog appeared — blocked on the user. Carries a
            // real `tool_name`, unlike the `Notification` permission_prompt whose
            // tool name is parsed out of a message string.
            let tool = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("tool");
            Some(Classification::new(
                Status::Blocked,
                Some(format!("needs approval: {}", tool)),
                format!("tool-permission dialog for {tool}; blocked on user"),
            ))
        }
        "Elicitation" => {
            // An MCP tool is requesting input — blocked on the user. The message
            // field is read defensively (confirmed empirically) with a fallback.
            let msg = ["message", "prompt", "title", "text", "elicitation"]
                .iter()
                .find_map(|k| payload.get(k).and_then(|v| v.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(clean_prompt)
                .map(|s| s.chars().take(60).collect::<String>())
                .filter(|s| !s.is_empty());
            Some(Classification::new(
                Status::Blocked,
                Some(msg.unwrap_or_else(|| "needs your input".into())),
                "MCP tool requested input; blocked on user",
            ))
        }
        "ElicitationResult" => {
            // The user answered the MCP elicitation that `Elicitation` marked the
            // row `Blocked` for — the authoritative unblock, fired the moment the
            // response is submitted. Carry the row back to `Working` immediately
            // instead of waiting for the transcript watcher to infer the resume.
            // `Blocked` → `Working` is a non-boundary, so the underlying task
            // label, `original_prompt`, and working accumulator are preserved
            // (and `status_before_working` captures `Blocked`, so an Esc-cancel of
            // the resumed turn reverts to the prompt). No label of its own.
            Some(Classification::new(Status::Working, None, "user answered the MCP elicitation; resuming"))
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
/// `confirm ` and `ready to ` (trailing space, so `confirmed` /
/// `confirmation` don't match, and `ready` alone with no follow-up doesn't)
/// catch approval prompts whose `?` isn't last —
/// (`"Confirm v0.5.0 and these notes? On approval I'll …"`,
///  `"Ready to tag v0.5.1 and push it? Reply with y …"`).
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
    "confirm ",
    "ready to ",
];

/// The two config-driven question heuristics, borrowed together because they
/// always travel as a pair from [`Config`]. `closers` are suffix-matched on the
/// final question (a polite sign-off like "What's next?"); `openers` are
/// prefix-matched on the last sentence (an optional offer like "Anything …?").
#[derive(Clone, Copy)]
pub(crate) struct QuestionRules<'a> {
    pub closers: &'a [String],
    pub openers: &'a [String],
}

impl<'a> QuestionRules<'a> {
    pub(crate) fn from_config(cfg: &'a Config) -> Self {
        Self { closers: &cfg.benign_closers, openers: &cfg.benign_openers }
    }
}

/// The final sentence of `text` — the segment after the last interior sentence
/// terminator. Used to test whether the closing question *opens* with a benign
/// offer word (e.g. "Anything …") rather than reading the whole message.
fn final_sentence(text: &str) -> &str {
    text.split(|c| matches!(c, '.' | '!' | '?' | '\n'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .last()
        .unwrap_or("")
}

/// True when `sentence` opens with one of the configured benign offer openers
/// (case-insensitive, after trimming) — marking the question an optional offer
/// to do more ("Anything you'd like to look at?") rather than a hand-back. An
/// embedded real ask isn't suppressed here: it's still caught downstream by the
/// permission-seeking path, so "Anything else, or shall I commit?" awaits.
fn opens_with_benign_offer(sentence: &str, benign_openers: &[String]) -> bool {
    let lower = sentence.trim_start().to_lowercase();
    benign_openers.iter().any(|o| !o.is_empty() && lower.starts_with(&o.to_lowercase()))
}

/// True when `text` reads as a hand-back to the user: the whole text ends with
/// `?` (possibly after a trailing option list); the last paragraph contains a
/// known hand-back phrase (permission-seeking or a direct second-person
/// question); the last paragraph issues a hand-back request for input
/// (`Paste …` / `Please provide …` / `Confirm …`); or the last paragraph
/// *opens* with a question that a concluding statement then follows.
///
/// Test-only wrapper over [`question_reason`] — production code calls
/// `question_reason` directly (it needs the matched-rule string for the decision
/// log). Kept `#[cfg(test)]` for the extensive question-detection unit tests.
#[cfg(test)]
pub(crate) fn is_a_question(text: &str, rules: QuestionRules) -> bool {
    question_reason(text, rules).is_some()
}

/// Like [`is_a_question`] but returns *which* rule fired (or `None` when the
/// text reads as a plain statement). The matched-rule string is recorded in the
/// decision log so an investigation can see why a turn was judged a question
/// without re-running the heuristics or reading the transcript.
pub(crate) fn question_reason(text: &str, rules: QuestionRules) -> Option<&'static str> {
    let plain = strip_markdown(text);
    let effective = strip_trailing_options(&plain);
    if effective.ends_with('?') {
        let lower = effective.to_lowercase();
        let is_benign_closer = rules.closers.iter().any(|c| {
            !c.is_empty() && lower.ends_with(&c.to_lowercase())
        });
        let is_benign_offer = opens_with_benign_offer(final_sentence(effective), rules.openers);
        if !is_benign_closer && !is_benign_offer {
            return Some("text ends with '?'");
        }
    }
    if has_permission_seeking_question(&plain) {
        return Some("permission-seeking phrase before a '?'");
    }
    if has_handback_request(&plain) {
        return Some("sentence-initial hand-back request");
    }
    if last_paragraph_opens_with_question(&plain, rules) {
        return Some("last paragraph opens with a question");
    }
    if handback_before_trailing_outro(&plain, rules) {
        return Some("hand-back question before a trailing outro");
    }
    None
}

/// A short single-line tail of assistant text for the decision log. The
/// hand-back is at the end of a message, so the *tail* is kept; chrome is
/// stripped via [`clean_prompt`] and the result capped so the log stays compact.
pub(crate) fn evidence_snippet(text: &str) -> String {
    const MAX: usize = 160;
    let cleaned = clean_prompt(text);
    let chars: Vec<char> = cleaned.chars().collect();
    if chars.len() <= MAX {
        cleaned
    } else {
        let tail: String = chars[chars.len() - MAX..].iter().collect();
        format!("…{tail}")
    }
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

/// True when `paragraph` carries a permission-seeking phrase positioned before a
/// later `?` (or a phrase that bakes in its own `?`). Shared by the last-paragraph
/// check and the trailing-outro look-back.
fn permission_seeking_in(paragraph: &str) -> bool {
    let lower = paragraph.to_lowercase();
    PERMISSION_SEEKING.iter().any(|phrase| {
        if let Some(phrase_start) = lower.find(phrase) {
            let after_phrase = &lower[phrase_start + phrase.len()..];
            phrase.ends_with('?') || after_phrase.contains('?')
        } else {
            false
        }
    })
}

/// Check the last paragraph for a permission-seeking phrase. A phrase ending in
/// `?` matches as-is; the rest must be followed by a `?` later in the paragraph.
/// Catches questions embedded mid-paragraph like "Want me to add that? The
/// plan: ..." where the response continues after the question.
fn has_permission_seeking_question(text: &str) -> bool {
    permission_seeking_in(last_paragraph(text))
}

/// Forward-reference connectives that open a *trailing outro* paragraph — the
/// agent stating what it will do once the user answers the hand-back question in
/// the paragraph just above ("…leave them as historical record?\n\nThen I'll
/// `/commit`."). The single-last-paragraph checks miss that question because it
/// isn't in the final paragraph. Gating the look-back on these openers (trailing
/// space, so `once` alone doesn't match) keeps a self-answered rhetorical
/// question ("Want me to fix it?\n\nI went ahead and fixed it.") — whose trailing
/// paragraph reports completed work and opens with none of these — correctly Done.
const TRAILING_OUTRO_OPENERS: &[&str] = &[
    "then i'll ",
    "then i can ",
    "once you ",
    "on approval",
    "after you ",
];

/// True when the paragraph *before* a trailing outro paragraph reads as a
/// hand-back — it ends in `?` (benign closers / offers still excused) or carries
/// a permission-seeking phrase before a `?`. Catches the "question?\n\nThen I'll
/// …" shape the single-last-paragraph checks miss, without disturbing the
/// self-answered case the outro-opener gate excludes.
fn handback_before_trailing_outro(text: &str, rules: QuestionRules) -> bool {
    let mut paras = text.rsplit("\n\n").map(str::trim).filter(|p| !p.is_empty());
    let Some(last) = paras.next() else { return false };
    let lower_last = last.to_lowercase();
    if !TRAILING_OUTRO_OPENERS.iter().any(|o| lower_last.starts_with(o)) {
        return false;
    }
    let Some(prev) = paras.next() else { return false };
    let effective = strip_trailing_options(prev);
    if effective.ends_with('?') {
        let lower = effective.to_lowercase();
        let benign = rules.closers.iter().any(|c| !c.is_empty() && lower.ends_with(&c.to_lowercase()))
            || opens_with_benign_offer(final_sentence(effective), rules.openers);
        if !benign {
            return true;
        }
    }
    permission_seeking_in(prev)
}

/// Sentence-initial imperative openers that hand control back to the user — the
/// agent is waiting for them to supply something. Kept narrow and phrase-matched
/// (not a blanket `"please "`) so informational openers like "Please note …" /
/// "Please see …" don't register as questions. `confirm ` (trailing space, so
/// `confirmed` doesn't match) catches approval prompts phrased without a `?` —
/// "Confirm to tag v1.2.0, or request edits." — which the `?`-gated
/// `PERMISSION_SEEKING` entry can't see.
const HANDBACK_OPENERS: &[&str] = &["paste ", "please provide ", "confirm "];

/// True when the last paragraph issues a sentence-initial hand-back request —
/// "Paste the output …", "Please provide the model name …" — meaning the agent
/// is waiting on the user. Only a sentence-initial imperative counts; a
/// mid-sentence mention like "you can paste this" or "I'll paste the result"
/// does not.
fn has_handback_request(text: &str) -> bool {
    last_paragraph(text)
        .to_lowercase()
        .split(|c| matches!(c, '.' | '!' | '?' | '\n'))
        .any(|sentence| {
            let s = sentence.trim_start();
            HANDBACK_OPENERS.iter().any(|opener| s.starts_with(opener))
        })
}

/// First-person openers that announce the agent is about to act itself rather
/// than hand back — so a leading question after one is the agent reasoning
/// aloud ("Let me investigate — does X have a cleaner fix? This affects …"),
/// not an ask. Matched at the start of the last paragraph's first sentence to
/// keep `last_paragraph_opens_with_question` from reading self-directed musing
/// as a hand-back. Trailing space avoids prefix collisions (`lets ` ≠ `let's`,
/// `i will ` ≠ `i willingly`).
const SELF_DIRECTED_OPENERS: &[&str] = &[
    "let me ",
    "let's ",
    "lets ",
    "i'll ",
    "i will ",
    "i'm going to ",
    "i am going to ",
];

/// True when the **first sentence** of the last paragraph is itself a question
/// — it ends with `?` before the paragraph continues with a concluding clause.
/// Catches a hand-back whose question leads and is then trailed by context,
/// like "Apply this edit? (yes / no) Everything else is aligned." — the trailing
/// sentence defeats the whole-text trailing-`?` path, and "apply" is no
/// permission-seeking phrase, so nothing else sees it. Three guards keep it from
/// over-firing: the terminating `?` must immediately follow an alphanumeric
/// character, so a bare mention of the glyph ("a `?` immediately followed by …",
/// which strip_markdown leaves as "a ? …") isn't read as a terminator; a
/// `SELF_DIRECTED_OPENERS` prefix ("Let me …", "I'll …") marks the leading
/// question as the agent musing rather than asking; and configured benign
/// closers are honored, so a polite "What's next? …" still doesn't flag.
/// Empirically fires on 12/60 real assistant turns with zero false positives.
fn last_paragraph_opens_with_question(text: &str, rules: QuestionRules) -> bool {
    let last_para = last_paragraph(text);
    let Some((term_idx, term_ch)) = last_para
        .char_indices()
        .find(|(_, c)| matches!(c, '.' | '!' | '?'))
    else {
        return false;
    };
    if term_ch != '?' {
        return false;
    }
    if !last_para[..term_idx].chars().next_back().is_some_and(char::is_alphanumeric) {
        return false;
    }
    let first_sentence = &last_para[..=term_idx];
    let lower = first_sentence.to_lowercase();
    if SELF_DIRECTED_OPENERS.iter().any(|o| lower.trim_start().starts_with(o)) {
        return false;
    }
    if opens_with_benign_offer(first_sentence, rules.openers) {
        return false;
    }
    !rules.closers.iter().any(|c| !c.is_empty() && lower.ends_with(&c.to_lowercase()))
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
    use std::path::Path;

    fn cfg_with(projects_root: Option<&str>, benign_closers: &[&str]) -> Config {
        let mut cfg = Config::default();
        cfg.projects_root = projects_root.map(str::to_string);
        cfg.benign_closers = benign_closers.iter().map(|s| s.to_string()).collect();
        cfg
    }

    /// No config-driven openers/closers — the bare heuristics only.
    const NO_RULES: QuestionRules = QuestionRules { closers: &[], openers: &[] };

    /// Borrow a closer list as [`QuestionRules`] with no openers, so the
    /// closer-suffix tests read the same as before the openers were added.
    fn with_closers(closers: &[String]) -> QuestionRules<'_> {
        QuestionRules { closers, openers: &[] }
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
        let (status, label) = classify("UserPromptSubmit", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn user_prompt_submit_with_blank_prompt_returns_working_without_label() {
        let p = json!({"prompt": "   "});
        let (status, label) = classify("UserPromptSubmit", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label, None);
    }

    #[test]
    fn user_prompt_submit_missing_prompt_returns_working_without_label() {
        let p = json!({});
        let (status, label) = classify("UserPromptSubmit", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Working);
        assert_eq!(label, None);
    }

    // ----- classify: Stop (from the `last_assistant_message` + `background_tasks` payload) -----

    #[test]
    fn stop_without_final_message_is_done() {
        // No `last_assistant_message` in the payload (e.g. a turn that ended on
        // tool use with no final text) → Done.
        let (status, label) = classify("Stop", &json!({}), NO_RULES).unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(label, None);
    }

    #[test]
    fn stop_with_question_final_message_is_blocked() {
        let p = json!({"last_assistant_message": "Should I proceed?"});
        let (status, label) = classify("Stop", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("has a question"));
    }

    #[test]
    fn stop_with_statement_final_message_is_done() {
        let p = json!({"last_assistant_message": "All tests passing."});
        let (status, label) = classify("Stop", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Done);
        assert_eq!(label, None);
    }

    #[test]
    fn stop_with_in_flight_background_tasks_is_waiting() {
        // Background work still running when the main turn settles → Waiting,
        // with no label so the prior task label is preserved.
        let p = json!({
            "last_assistant_message": "Kicked off the batch.",
            "background_tasks": [{"id": "1", "type": "shell", "status": "running", "description": "build"}]
        });
        let (status, label) = classify("Stop", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Waiting);
        assert_eq!(label, None);
    }

    #[test]
    fn stop_with_empty_background_tasks_array_is_done() {
        let p = json!({"last_assistant_message": "Done.", "background_tasks": []});
        let (status, _) = classify("Stop", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Done);
    }

    #[test]
    fn stop_question_takes_precedence_over_background_tasks() {
        // A hand-back question wins over Waiting — the user needs to act, even
        // while background work continues.
        let p = json!({
            "last_assistant_message": "Should I deploy?",
            "background_tasks": [{"id": "1", "type": "shell", "status": "running"}]
        });
        let (status, label) = classify("Stop", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("has a question"));
    }

    // ----- classify: Notification -----

    #[test]
    fn notification_permission_prompt_extracts_tool() {
        let p = json!({
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"
        });
        let (status, label) = classify("Notification", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("needs approval: Bash"));
    }

    #[test]
    fn notification_plan_approval_fixed_label() {
        let p = json!({"notification_type": "plan_approval", "message": "ignored"});
        let (_, label) = classify("Notification", &p, NO_RULES).unwrap();
        assert_eq!(label.as_deref(), Some("plan approval"));
    }

    #[test]
    fn notification_idle_prompt_is_ignored() {
        // `idle_prompt` is no longer classified — `Stop` already settled the row,
        // so the redundant (and flaky) transcript re-scan was removed.
        let p = json!({"notification_type": "idle_prompt"});
        assert!(classify("Notification", &p, NO_RULES).is_none());
    }

    #[test]
    fn notification_without_type_but_with_message_is_blocked() {
        let p = json!({"message": "Claude needs your attention"});
        let (status, label) = classify("Notification", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("Claude needs your attention"));
    }

    #[test]
    fn notification_label_truncates_to_60_chars() {
        let p = json!({"notification_type": "attention", "message": "y".repeat(200)});
        let (_, label) = classify("Notification", &p, NO_RULES).unwrap();
        assert_eq!(label.unwrap().chars().count(), 60);
    }

    #[test]
    fn notification_message_strips_terminal_chrome() {
        let p = json!({"message": "⎿  Error: pattern blocked"});
        let (_, label) = classify("Notification", &p, NO_RULES).unwrap();
        assert_eq!(label.as_deref(), Some("Error: pattern blocked"));
    }

    // ----- classify: PreToolUse -----

    #[test]
    fn pre_tool_use_ask_user_question_is_blocked_with_question_label() {
        let p = json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "?"}]}});
        let (status, label) = classify("PreToolUse", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("has a question"));
    }

    #[test]
    fn pre_tool_use_exit_plan_mode_is_blocked_with_plan_label() {
        let p = json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "..."}});
        let (status, label) = classify("PreToolUse", &p, NO_RULES).unwrap();
        assert_eq!(status, Status::Blocked);
        assert_eq!(label.as_deref(), Some("plan approval"));
    }

    #[test]
    fn pre_tool_use_for_unrelated_tool_is_ignored() {
        let p = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        assert!(classify("PreToolUse", &p, NO_RULES).is_none());
    }

    #[test]
    fn pre_tool_use_without_tool_name_is_ignored() {
        assert!(classify("PreToolUse", &json!({}), NO_RULES).is_none());
    }

    // ----- classify: SessionStart -----

    #[test]
    fn session_start_with_no_fields_is_idle() {
        let (status, label) = classify("SessionStart", &json!({}), NO_RULES).unwrap();
        assert_eq!(status, Status::Idle);
        assert_eq!(label, None);
    }

    // ----- classify: unknown -----

    #[test]
    fn unknown_event_returns_none() {
        assert!(classify("PreToolUse", &json!({}), NO_RULES).is_none());
    }

    // ----- question_reason / evidence_snippet (decision-log evidence) -----

    #[test]
    fn question_reason_names_the_matched_rule() {
        assert_eq!(question_reason("Should I proceed?", NO_RULES), Some("text ends with '?'"));
        assert_eq!(
            question_reason("Want me to add that? The plan continues here.", NO_RULES),
            Some("permission-seeking phrase before a '?'"),
        );
        assert_eq!(
            question_reason("Paste the full error output here.", NO_RULES),
            Some("sentence-initial hand-back request"),
        );
        assert_eq!(question_reason("All done. Everything passed.", NO_RULES), None);
    }

    #[test]
    fn evidence_snippet_keeps_short_text_and_tails_long_text() {
        assert_eq!(evidence_snippet("All done."), "All done.");
        let long = format!("{} Push?", "x".repeat(400));
        let snip = evidence_snippet(&long);
        assert!(snip.starts_with('…'), "long text is tail-truncated with a leading ellipsis");
        assert!(snip.ends_with("Push?"), "the trailing question survives truncation");
        assert!(snip.chars().count() <= 161);
    }

    // ----- is_a_question -----

    #[test]
    fn is_a_question_simple_question_mark() {
        assert!(is_a_question("Should I proceed?", NO_RULES));
    }

    #[test]
    fn is_a_question_no_question_mark() {
        assert!(!is_a_question("All done.", NO_RULES));
    }

    #[test]
    fn is_a_question_empty_text() {
        assert!(!is_a_question("", NO_RULES));
    }

    #[test]
    fn is_a_question_strips_trailing_option_list() {
        assert!(is_a_question(
            "Save these? (all / numbers / none)",
            NO_RULES
        ));
    }

    #[test]
    fn is_a_question_bold_wrapped_question_is_detected() {
        // Regression: a final "**Push?**" must read as a question. The trailing
        // "**" used to hide the "?" so the row went Done instead of Blocked.
        assert!(is_a_question(
            "Remote was in sync, so this will fast-forward cleanly.\n\n**Push?**",
            NO_RULES
        ));
    }

    #[test]
    fn is_a_question_other_markdown_emphasis_is_stripped() {
        assert!(is_a_question("*continue?*", NO_RULES));
        assert!(is_a_question("`run this?`", NO_RULES));
        assert!(is_a_question("__ready to deploy?__", NO_RULES));
        assert!(is_a_question("## Proceed?", NO_RULES));
    }

    #[test]
    fn is_a_question_bold_question_with_trailing_options() {
        assert!(is_a_question("**Save these?** (all / none)", NO_RULES));
    }

    #[test]
    fn is_a_question_bold_statement_is_not_a_question() {
        assert!(!is_a_question("**All done.**", NO_RULES));
    }

    #[test]
    fn is_a_question_strips_trailing_option_list_with_extra_whitespace() {
        assert!(is_a_question(
            "Continue?   (yes / no)  \n",
            NO_RULES
        ));
    }

    #[test]
    fn is_a_question_does_not_strip_unrelated_parens() {
        // Trailing "(foo.py)" doesn't follow a "?", so we don't strip and the
        // text doesn't end with "?" → not a question.
        assert!(!is_a_question("Look at this code (foo.py)", NO_RULES));
    }

    #[test]
    fn is_a_question_keeps_inline_parens() {
        assert!(is_a_question("Should I update foo (the helper)?", NO_RULES));
    }

    #[test]
    fn is_a_question_benign_closer_with_options_is_not_a_question() {
        let closers = vec!["What's next?".to_string()];
        assert!(!is_a_question("What's next? (continue / stop)", with_closers(&closers)));
    }

    #[test]
    fn is_a_question_benign_closer_alone_is_not_a_question() {
        let closers = vec!["What's next?".to_string()];
        for text in ["What's next?", "what's next?", "Done. What's next?"] {
            assert!(!is_a_question(text, with_closers(&closers)), "text: {}", text);
        }
    }

    #[test]
    fn is_a_question_non_matching_closer_still_awaits() {
        let closers = vec!["What's next?".to_string()];
        assert!(is_a_question("Which option do you prefer?", with_closers(&closers)));
    }

    // ----- benign openers (offer questions) -----

    fn with_openers(openers: &[String]) -> QuestionRules<'_> {
        QuestionRules { closers: &[], openers }
    }

    #[test]
    fn benign_opener_offer_is_not_a_question() {
        // A question whose final sentence opens with "anything" is an optional
        // offer to do more, not a hand-back — every one of these is drawn from
        // real assistant sign-offs in prompt_history.json.
        let openers = vec!["anything".to_string()];
        for text in [
            "Anything you'd like to look at?",
            "Anything you'd like me to pick up from here?",
            "anything to adjust?",
            "I've finished the refactor and tests pass.\n\nAnything else?",
        ] {
            assert!(!is_a_question(text, with_openers(&openers)), "text: {}", text);
        }
    }

    #[test]
    fn benign_opener_does_not_suppress_embedded_ask() {
        // An offer that also poses a real decision still awaits: the bare-`?`
        // path is skipped, but the permission-seeking path catches "shall i" /
        // "ready to" even though the sentence opens with a benign offer word.
        let openers = vec!["anything".to_string()];
        assert!(is_a_question("Anything else, or shall I commit the batch?", with_openers(&openers)));
        assert!(is_a_question("Anything else, or ready to commit?", with_openers(&openers)));
    }

    #[test]
    fn benign_opener_only_applies_to_the_final_sentence() {
        // An earlier "Anything…" mention must not neutralize a genuine closing
        // question — only the final sentence's opener is consulted.
        let openers = vec!["anything".to_string()];
        assert!(is_a_question("Anything goes here. Should I proceed?", with_openers(&openers)));
    }

    #[test]
    fn benign_opener_default_config_neutralizes_offer() {
        // The shipped default opener list is `["anything"]`, so a fresh config
        // treats the offer as Done without any user tuning.
        let cfg = Config::default();
        assert!(!is_a_question("Anything you'd like to look at?", QuestionRules::from_config(&cfg)));
    }

    // ----- permission-seeking in last paragraph -----

    #[test]
    fn permission_seeking_want_me_to_mid_paragraph() {
        assert!(is_a_question(
            "The state is ephemeral. Want me to add persistence? The plan: write sessions.json to disk.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_shall_i_mid_paragraph() {
        assert!(is_a_question(
            "Three changes here. Shall I proceed? I'll create separate commits.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_should_i_mid_paragraph() {
        assert!(is_a_question(
            "Found the issue. Should I use the cached value? It would avoid the network call.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_do_you_want_mid_paragraph() {
        assert!(is_a_question(
            "Deployed. Do you want me to run the tests? I can also check coverage.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_ready_to_mid_paragraph() {
        // "Ready to <action>?" approval prompts whose "?" isn't last —
        // ("Ready to tag v0.5.1 and push it? Reply with y …").
        assert!(is_a_question(
            "Ready to tag v0.5.1 and push it? Reply with y to tag + push, or tell me what to tweak in the notes first.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_save_prompt_with_trailing_clause() {
        // The /reflect and /commit skills close with "Save this/these? (...)"
        // menus a trailing clause can follow, so the text doesn't end with "?".
        // The "save this?"/"save these?" phrasing flips it to blocked.
        assert!(is_a_question(
            "Save this? (all / 1 / none) — then I'll run /commit.",
            NO_RULES
        ));
        assert!(is_a_question(
            "Save these? (all / numbers / none) — I'll commit after.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_save_without_question_mark_is_not_blocked() {
        // The "?" is baked into the phrase, so a declarative "save this ..."
        // must not match — only the literal "save this?" question does.
        assert!(!is_a_question(
            "Let me save this config before continuing. Running the build now.",
            NO_RULES
        ));
    }

    #[test]
    fn directed_question_mid_paragraph_is_blocked() {
        // Direct second-person questions whose paragraph continues past the "?".
        for text in [
            "Can you reopen the history and confirm the line is there? If not, I'll dig in.",
            "Could you paste that line? It'll look like a JSON blob.",
            "Did you try the admin launch? That's the most likely fix.",
            "Want to try a clean test? Reset the config and relaunch.",
        ] {
            assert!(is_a_question(text, NO_RULES), "text: {}", text);
        }
    }

    #[test]
    fn confirm_prompt_with_question_not_last_is_blocked() {
        // Approval prompt whose "?" is followed by a plan/tail — the exact
        // shape of the release skill's "Confirm vX? On approval I'll …".
        assert!(is_a_question(
            "Confirm v0.5.0 and these notes? On approval I'll tag and push.",
            NO_RULES
        ));
        // Trailing space guards against `confirmed` / `confirmation` matching
        // a declarative sentence that happens to share a paragraph with a "?".
        // (Bare "confirm" would false-positive here; "confirm " does not, and
        // the text doesn't end with "?" so the trailing-? path stays quiet.)
        assert!(!is_a_question(
            "The change is confirmed working. Does that match? Shipping it now.",
            NO_RULES
        ));
    }

    #[test]
    fn paste_request_is_blocked() {
        // A sentence-initial "Paste ..." imperative means the agent is waiting.
        assert!(is_a_question("Paste the tableinfos output and I'll finish arena.", NO_RULES));
        assert!(is_a_question("Looks good. Paste whatever it prints.", NO_RULES));
    }

    #[test]
    fn paste_mention_mid_sentence_is_not_blocked() {
        // Only a sentence-initial imperative counts — a mention does not.
        assert!(!is_a_question("You can paste this into the terminal later. All set.", NO_RULES));
        assert!(!is_a_question("I'll paste the result here once it's done.", NO_RULES));
    }

    #[test]
    fn please_provide_request_is_blocked() {
        // A sentence-initial "Please provide ..." imperative hands back to the user.
        assert!(is_a_question(
            "Please provide the model group (e.g. `other`, `inserts`) and the model name.",
            NO_RULES
        ));
        assert!(is_a_question("Looks good. Please provide your API key.", NO_RULES));
    }

    #[test]
    fn confirm_request_without_question_mark_is_blocked() {
        // A sentence-initial "Confirm ..." imperative hands back even without a `?`.
        assert!(is_a_question("Confirm to tag v1.2.0, or request edits.", NO_RULES));
        assert!(is_a_question(
            "Skipped as internal: docs and the memory chore. Confirm to tag v1.2.0, or request edits.",
            NO_RULES
        ));
        // "Confirmed ..." is a statement, not a hand-back — trailing space keeps it out.
        assert!(!is_a_question("Confirmed: the fix works on both platforms.", NO_RULES));
    }

    #[test]
    fn please_note_is_not_blocked() {
        // Informational "Please ..." openers must not register as hand-backs.
        assert!(!is_a_question("Please note the migration runs on next launch.", NO_RULES));
        assert!(!is_a_question("Done. Please see the updated README for details.", NO_RULES));
    }

    #[test]
    fn permission_seeking_only_checks_last_paragraph() {
        // Question in first paragraph, statement in last — should NOT match.
        assert!(!is_a_question(
            "Want me to fix it?\n\nI went ahead and fixed it. All tests pass.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_case_insensitive() {
        assert!(is_a_question(
            "WANT ME TO add this? Here's the plan.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_no_question_mark_after_phrase() {
        assert!(!is_a_question(
            "I want me to clarify: the fix is in place. All done.",
            NO_RULES
        ));
    }

    #[test]
    fn permission_seeking_does_not_match_unrelated_question_in_last_para() {
        // A self-directed musing question ("Let me investigate — does …?") in
        // the last paragraph must not register: no permission-seeking phrase,
        // the text doesn't end with "?", and the leading-question path is
        // excluded by the SELF_DIRECTED_OPENERS guard ("Let me ").
        assert!(!is_a_question(
            "Let me investigate — does the bug have a cleaner fix at the rotate_vector level? This affects what we do next.",
            NO_RULES
        ));
    }

    // ----- leading question in last paragraph -----

    #[test]
    fn leading_question_with_trailing_sentence_is_a_question() {
        // The reported bug: a hand-back whose question leads, then a closing
        // statement follows — so the whole text doesn't end with "?" and no
        // permission-seeking phrase matches. The first-sentence path catches it.
        assert!(is_a_question(
            "Apply this one edit? (yes / no) Everything else is already aligned.",
            NO_RULES
        ));
    }

    #[test]
    fn leading_question_self_directed_opener_is_not_a_question() {
        // Same shape as the bug, but a first-person opener marks it as the agent
        // about to act, not asking. These must stay Done.
        for text in [
            "Let me check the logs first? Actually, I'll just run it.",
            "I'll refactor this? No — splitting it is cleaner. Doing that now.",
        ] {
            assert!(!is_a_question(text, NO_RULES), "text: {}", text);
        }
    }

    #[test]
    fn leading_question_bare_glyph_mention_is_not_a_question() {
        // A literal mention of the "?" glyph (after markdown strip, "a ? ...")
        // isn't a sentence terminator — the char before "?" is a space.
        assert!(!is_a_question(
            "I added a path for text ending in a `?` followed by an option menu. Done.",
            NO_RULES
        ));
    }

    #[test]
    fn leading_question_benign_closer_is_not_a_question() {
        let closers = vec!["What's next?".to_string()];
        assert!(!is_a_question(
            "What's next? I'll wait for your call.",
            with_closers(&closers)
        ));
    }

    #[test]
    fn leading_question_path_requires_the_first_sentence_to_be_the_question() {
        // Contract of the helper itself: when the first sentence is a statement
        // and the question comes second, this path stays out (other paths own
        // the trailing-"?" / permission-phrase cases). Tested directly because
        // through `is_a_question` a trailing "?" would mask it via path 1.
        assert!(!last_paragraph_opens_with_question(
            "The migration is ready. Looks good to you?",
            NO_RULES
        ));
        assert!(last_paragraph_opens_with_question(
            "Looks good to you? The migration is ready.",
            NO_RULES
        ));
    }

    // ----- hand-back question before a trailing outro -----

    #[test]
    fn handback_question_before_trailing_outro_is_a_question() {
        // The real corpus miss: a hand-back whose question sits one paragraph up,
        // trailed by a standalone "Then I'll …" outro. The single-last-paragraph
        // checks miss it because the final paragraph is the outro, not the question.
        assert!(is_a_question(
            "## One thing left — your call\n\nWant me to (a) sweep all the notes, (b) just fix the 2 stale ones, or (c) leave them as historical record?\n\nThen I'll /commit — which closes the last memo.",
            NO_RULES
        ));
        // Minimal shapes, both outro openers.
        assert!(is_a_question("Should I use the cached value?\n\nThen I'll run the tests.", NO_RULES));
        assert!(is_a_question("Want me to proceed?\n\nOnce you confirm I'll push.", NO_RULES));
    }

    #[test]
    fn self_answered_question_before_outro_stays_done() {
        // The trailing paragraph reports completed work and opens with none of the
        // outro connectives, so the rhetorical question two paragraphs up must NOT
        // flip the row to a question. (Same fixture as the last-paragraph guard.)
        assert!(!is_a_question(
            "Want me to fix it?\n\nI went ahead and fixed it. All tests pass.",
            NO_RULES
        ));
    }

    #[test]
    fn trailing_outro_without_a_question_above_stays_done() {
        // A "Then I'll …" outro after a plain statement is not a hand-back.
        assert!(!is_a_question(
            "The refactor is complete and lint is clean.\n\nThen I'll move on to the tests.",
            NO_RULES
        ));
    }

    #[test]
    fn benign_closer_before_outro_stays_done() {
        // A benign closer two paragraphs up is still excused even with an outro.
        let closers = vec!["What's next?".to_string()];
        assert!(!is_a_question(
            "Everything is deployed.\n\nWhat's next?\n\nThen I'll wait for your call.",
            with_closers(&closers)
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
    fn dispatch_stop_failure_sets_error_with_reason() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo", "reason": "overloaded" });
        match dispatch("StopFailure", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.status, Status::Error);
                assert_eq!(input.label.as_deref(), Some("overloaded"));
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_stop_failure_without_reason_falls_back() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo" });
        match dispatch("StopFailure", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.status, Status::Error);
                assert_eq!(input.label.as_deref(), Some("turn failed"));
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_permission_request_awaits_with_tool() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo", "tool_name": "Bash" });
        match dispatch("PermissionRequest", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.status, Status::Blocked);
                assert_eq!(input.label.as_deref(), Some("needs approval: Bash"));
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_elicitation_awaits() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo", "message": "Pick a branch" });
        match dispatch("Elicitation", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.status, Status::Blocked);
                assert_eq!(input.label.as_deref(), Some("Pick a branch"));
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_elicitation_result_resumes_working() {
        // The user answered the MCP prompt — the row leaves Blocked for Working
        // with no label of its own (the task label is preserved downstream).
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo", "tool_name": "ask", "user_response": {"branch": "main"} });
        match dispatch("ElicitationResult", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.status, Status::Working);
                assert_eq!(input.label, None);
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_precompact_marks_boundary() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({ "cwd": "d:/projects/demo", "trigger": "manual" });
        match dispatch("PreCompact", &p, &cfg) {
            AdapterOutput::Boundary { id } => assert_eq!(id, "demo"),
            _ => panic!("expected Boundary"),
        }
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
            AdapterOutput::Set { input, transcript_path, .. } => {
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
    fn dispatch_slash_command_expansion_sets_working_no_dialog() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({
            "cwd": "d:/projects/demo",
            "session_id": "s",
            "expansion_type": "slash_command",
            "command_name": "commit",
            "command_args": "",
            "prompt": "/commit",
        });
        match dispatch("UserPromptExpansion", &p, &cfg) {
            AdapterOutput::Set { input, .. } => {
                assert_eq!(input.id, "demo");
                assert_eq!(input.status, Status::Working);
                assert_eq!(input.label.as_deref(), Some("/commit"));
                // UserPromptSubmit owns the dialog entry; expansion adds none.
                assert!(input.dialog_entry.is_none());
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn dispatch_non_slash_expansion_is_ignored() {
        let cfg = cfg_with(Some("d:/projects"), &[]);
        let p = json!({
            "cwd": "d:/projects/demo",
            "expansion_type": "file_reference",
            "prompt": "@foo.rs",
        });
        assert!(matches!(
            dispatch("UserPromptExpansion", &p, &cfg),
            AdapterOutput::Ignore
        ));
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
