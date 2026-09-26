//! Releases a subagent's permission prompt once its own transcript records how
//! the dialog ended.
//!
//! A `PermissionRequest` raised by a subagent opens a prompt over its row (see
//! [`crate::state::AgentSession::subagent_gate`]), and nothing in the hook
//! stream says when that dialog closes. Claude Code does record it, though: the
//! gated call's `tool_result` lands in `agent-<agent_id>.jsonl` for every
//! outcome — approved and run, rejected by the user, the 120s safety-check
//! timeout, an abort. This tick finds that result and settles the prompt.
//!
//! The hook carries no `tool_use_id`, so the call is identified by what it does
//! carry: the tool's name, its input, and when it arrived. [`gate_verdict`]
//! takes the call whose input matches the hook's exactly, and with none, every
//! same-name call in the newest message that already held one when the hook
//! fired. It settles only once every candidate has a result — erring toward
//! holding, since an early release hides a dialog that is still on screen.
//!
//! Three hook exits settle prompts without this module, each at a moment the
//! subagent can no longer be prompting: its `SubagentStop`, a main `Stop` with
//! no background work in flight from the session that raised it, and removal
//! of the row. No timer releases a prompt.
//!
//! Every tick re-reads the whole transcript once it has grown, never only the
//! new tail, so a result written before the hook arrived (a fast approval) is
//! still seen. The sessions lock is never held across the file IO.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::commands::{emit_sessions_updated, now_ms};
use crate::log_watcher::TranscriptEntry;
use crate::state::{AppState, PendingPrompt, SettleOutcome, SettleScope};
use crate::token_scan::{parse_rfc3339_ms, transcript_files};

/// Tick cadence. The false pings this exists to prevent came 17s and 38s after
/// the dialog closed, so 2s is well inside them, and a result stays in the file
/// so a coarser tick can never miss one.
const POLL: Duration = Duration::from_secs(2);

/// How long after the hook a prompt is first judged, so the sidechain's own
/// ~100ms flush has written the gated `tool_use` before anyone looks for it.
const JUDGE_AFTER_MS: i64 = 1_000;

/// A `tool_use` whose message is first stamped up to this long after the hook
/// still counts as issued before it: the hook and the transcript are stamped by
/// different writers, and the call can be flushed a moment late.
const ISSUE_SKEW_MS: i64 = 2_000;

/// How far before the hook a call's result may be stamped and still belong to
/// this dialog: the hook runs a Python process, a 2s POST timeout and the
/// scheduler before it is applied, and a fast approval writes its result inside
/// that gap. The previous identical call's result in the incident files sat
/// 8.2s and 11s before the hook.
const HOOK_LATENCY_MS: i64 = 5_000;

/// How long a prompt waits before "nothing in the transcript names this call"
/// is logged — once per request, and only as a diagnosis: the prompt stays open.
const UNMATCHED_LOG_AFTER_MS: i64 = 10_000;

/// What a subagent's `PermissionRequest` said about the call it gates.
pub(crate) struct GateQuery<'a> {
    pub tool_name: &'a str,
    pub tool_input: &'a Value,
    /// When the hook was applied.
    pub requested_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GateVerdict {
    /// The gated call is in the transcript and has no result yet.
    Pending,
    /// No message in the transcript holds a call this dialog could be gating.
    Unmatched,
    /// Every candidate call has its result.
    Settled(GateMatch),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GateMatch {
    pub tool_use_ids: Vec<String>,
    /// Whether any of them failed — a rejection or a timeout included.
    pub is_error: bool,
    pub matched_on: MatchedOn,
}

/// How the gated call was told apart from its siblings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchedOn {
    /// The hook's `tool_input` equals the call's input exactly.
    Input,
    /// No input matched, so every same-name call in the message stood in.
    Name,
}

impl MatchedOn {
    fn as_str(self) -> &'static str {
        match self {
            MatchedOn::Input => "input",
            MatchedOn::Name => "name",
        }
    }
}

/// Which exit settled a prompt, for its decision line.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SettledVia {
    /// The agent's transcript recorded the gated call's result.
    ToolResult,
    /// The agent's `SubagentStop` arrived.
    SubagentStop,
    /// A main `Stop` reported no background work in flight.
    StopNoBackground,
}

impl SettledVia {
    fn as_str(self) -> &'static str {
        match self {
            SettledVia::ToolResult => "tool_result",
            SettledVia::SubagentStop => "subagent_stop",
            SettledVia::StopNoBackground => "stop_no_background",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            SettledVia::ToolResult => "the subagent's transcript recorded the gated call's result, so its dialog has closed",
            SettledVia::SubagentStop => "the subagent ended, so none of its dialogs can still be open",
            SettledVia::StopNoBackground => "the session's main turn ended with no background work in flight, so none of its subagents can still be prompting",
        }
    }
}

/// Why a prompt's call could not be found, logged once per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unmatched {
    /// The hook carried no `transcript_path`, so there is no file to read.
    NoTranscriptPath,
    /// No `agent-<id>.jsonl` exists under the session's `subagents/`.
    NoTranscript,
    /// The file exists and no message in it holds a call this dialog could gate.
    NoToolUse,
}

impl Unmatched {
    fn as_str(self) -> &'static str {
        match self {
            Unmatched::NoTranscriptPath => "no_transcript_path",
            Unmatched::NoTranscript => "no_transcript",
            Unmatched::NoToolUse => "no_tool_use",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Unmatched::NoTranscriptPath => "the hook carried no transcript_path; only SubagentStop, a main Stop with no background work, or removing the row can release this prompt",
            Unmatched::NoTranscript => "no transcript for this agent under the session's subagents directory yet; the prompt stays open",
            Unmatched::NoToolUse => "the agent's transcript holds no call this dialog could be gating; the prompt stays open",
        }
    }
}

/// Decide from a subagent's transcript whether the call its dialog gates has
/// its result yet. Pure, so every ordering the files have shown is testable.
///
/// Each line is parsed with the watcher's wire types; malformed lines and lines
/// with no timestamp are skipped. A call is *eligible* when it has the query's
/// tool name, its message was first stamped no later than
/// `requested_at + ISSUE_SKEW_MS`, and it has either no result or a result not
/// older than `requested_at - HOOK_LATENCY_MS` — which is what keeps an
/// identical call resolved in an earlier message from standing in for this one.
///
/// An eligible call whose input equals the query's is the gated one, and among
/// several the newest message's are waited on. Only with no exact match does
/// recency pick: every eligible call in the newest message stands in. Matching
/// input first is what keeps a fast approval from latching onto the agent's
/// *next* same-name call, stamped inside the skew window, and holding the BLOCK
/// until that unrelated call returns.
pub(crate) fn gate_verdict(lines: &[&str], q: &GateQuery) -> GateVerdict {
    struct Use {
        id: String,
        input: Option<Value>,
        msg: String,
    }
    let mut uses: Vec<Use> = Vec::new();
    let mut first_seen: HashMap<String, i64> = HashMap::new();
    let mut results: HashMap<String, (i64, bool)> = HashMap::new();
    for line in lines {
        let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) else { continue };
        let Some(ts) = entry.timestamp.as_deref().and_then(parse_rfc3339_ms) else { continue };
        let Some(message) = entry.message else { continue };
        let blocks = message.content.unwrap_or_default();
        match entry.entry_type.as_str() {
            "assistant" => {
                // One entry per content block, all sharing `message.id`; the
                // entry's own uuid stands in only where that id is missing.
                let Some(msg) = message.id.or(entry.uuid) else { continue };
                let seen = first_seen.entry(msg.clone()).or_insert(ts);
                *seen = (*seen).min(ts);
                for b in blocks.into_iter().filter(|b| b.block_type == "tool_use" && b.name.as_deref() == Some(q.tool_name)) {
                    if let Some(id) = b.id.filter(|id| !uses.iter().any(|u| &u.id == id)) {
                        uses.push(Use { id, input: b.input, msg: msg.clone() });
                    }
                }
            }
            "user" => {
                for b in blocks.into_iter().filter(|b| b.block_type == "tool_result") {
                    if let Some(id) = b.tool_use_id {
                        results.insert(id, (ts, b.is_error.unwrap_or(false)));
                    }
                }
            }
            _ => {}
        }
    }

    let issued = |u: &Use| first_seen.get(&u.msg).copied().filter(|t| *t <= q.requested_at + ISSUE_SKEW_MS);
    let eligible: Vec<(&Use, i64)> = uses
        .iter()
        .filter(|u| results.get(&u.id).is_none_or(|(ts, _)| *ts >= q.requested_at - HOOK_LATENCY_MS))
        .filter_map(|u| issued(u).map(|t| (u, t)))
        .collect();
    // An absent input and a null one both say "no input"; serde reads a JSON
    // `null` into `None`, so the two have to compare equal here.
    let exact: Vec<(&Use, i64)> = eligible.iter().copied().filter(|(u, _)| u.input.as_ref().unwrap_or(&Value::Null) == q.tool_input).collect();
    let (pool, matched_on) = if exact.is_empty() { (eligible, MatchedOn::Name) } else { (exact, MatchedOn::Input) };
    let Some(newest) = pool.iter().map(|(_, t)| *t).max() else { return GateVerdict::Unmatched };
    let Some(gated) = pool.iter().find(|(_, t)| *t == newest).map(|(u, _)| &u.msg) else { return GateVerdict::Unmatched };
    let chosen: Vec<&Use> = pool.iter().filter(|(u, _)| &u.msg == gated).map(|(u, _)| *u).collect();

    let mut tool_use_ids = Vec::with_capacity(chosen.len());
    let mut is_error = false;
    for u in chosen {
        let Some((_, failed)) = results.get(&u.id) else { return GateVerdict::Pending };
        is_error |= *failed;
        tool_use_ids.push(u.id.clone());
    }
    GateVerdict::Settled(GateMatch { tool_use_ids, is_error, matched_on })
}

/// The transcript of subagent `agent_id` under a session's `subagents/`
/// directory, at any depth: a workflow's agents sit under `workflows/<run>/`.
pub(crate) fn find_agent_transcript(dir: &Path, agent_id: &str) -> Option<PathBuf> {
    let name = format!("agent-{agent_id}.jsonl");
    transcript_files(dir).into_iter().find(|p| p.file_name().is_some_and(|n| n == name.as_str()))
}

/// Write the `subagent_prompt_settled` decision line — the one place it is
/// built, shared by the tick and by `http_server`'s two hook exits.
pub(crate) fn log_prompt_settled(chat_id: &str, o: &SettleOutcome, via: SettledVia, m: Option<&GateMatch>, now: i64) {
    let requests = o.settled.iter().map(|p| p.request.to_string()).collect::<Vec<_>>().join(",");
    let mut agents: Vec<&str> = Vec::new();
    for p in &o.settled {
        if !agents.contains(&p.prompt.agent_id.as_str()) {
            agents.push(&p.prompt.agent_id);
        }
    }
    let agent_ids = agents.join(",");
    // Every `requested_at` is at or before `now`, so folding from `now` yields the oldest.
    let latency_ms = now - o.settled.iter().map(|p| p.requested_at).fold(now, i64::min);
    let tool_use_ids = m.map(|m| m.tool_use_ids.join(","));
    let after = if o.released { "the row shows the main agent's own state again" } else { "other subagent prompts keep the row blocked" };
    let reason = format!("{}; {after}", via.reason());
    tracing::debug!(
        chat_id = %chat_id,
        decision = "subagent_prompt_settled",
        requests = %requests,
        agent_ids = %agent_ids,
        via = via.as_str(),
        released = o.released,
        remaining = o.remaining,
        status = ?o.status,
        latency_ms,
        tool_use_ids = tool_use_ids.as_deref(),
        is_error = m.map(|m| m.is_error),
        matched_on = m.map(|m| m.matched_on.as_str()),
        reason = %reason,
        "subagent prompt settled"
    );
}

/// What the tick remembers about one open prompt between passes.
#[derive(Default)]
struct Tracked {
    /// The agent's transcript, once found. Resolved once: the file does not move.
    path: Option<PathBuf>,
    /// The file's length at the last read, so an unchanged file is not re-read.
    last_len: Option<u64>,
    /// Whether that read found no candidate call — the cached verdict, since a
    /// `Settled` one is acted on and never needs remembering.
    last_unmatched: bool,
    /// Whether this prompt's `subagent_prompt_unmatched` line was written.
    unmatched_logged: bool,
}

impl Tracked {
    fn unmatched_once(&mut self, why: Unmatched) -> Option<Finding> {
        if self.unmatched_logged {
            return None;
        }
        self.unmatched_logged = true;
        Some(Finding::Unmatched(why))
    }
}

/// What one pass learned about one open prompt.
#[derive(Debug, PartialEq)]
enum Finding {
    Settled(GateMatch),
    Unmatched(Unmatched),
}

/// Judge one open prompt against its agent's transcript. Blocking file IO, so
/// it runs off the async runtime and never under the sessions lock.
fn judge(p: &PendingPrompt, t: &mut Tracked, now: i64) -> Option<Finding> {
    let waited = now - p.requested_at;
    if waited < JUDGE_AFTER_MS {
        return None;
    }
    let Some(dir) = p.prompt.subagents_dir.as_deref() else { return t.unmatched_once(Unmatched::NoTranscriptPath) };
    if t.path.is_none() {
        t.path = find_agent_transcript(dir, &p.prompt.agent_id);
    }
    let Some(path) = t.path.as_deref() else {
        return if waited >= UNMATCHED_LOG_AFTER_MS { t.unmatched_once(Unmatched::NoTranscript) } else { None };
    };
    let len = std::fs::metadata(path).ok()?.len();
    let unmatched = if t.last_len == Some(len) {
        t.last_unmatched
    } else {
        let bytes = std::fs::read(path).ok()?;
        let contents = String::from_utf8_lossy(&bytes);
        // Complete lines only: a line still being flushed is judged next pass.
        let complete = &contents[..contents.rfind('\n').map_or(0, |i| i + 1)];
        let lines: Vec<&str> = complete.lines().collect();
        t.last_len = Some(len);
        let query = GateQuery { tool_name: &p.prompt.tool_name, tool_input: &p.prompt.tool_input, requested_at: p.requested_at };
        match gate_verdict(&lines, &query) {
            GateVerdict::Settled(m) => return Some(Finding::Settled(m)),
            GateVerdict::Pending => false,
            GateVerdict::Unmatched => true,
        }
    };
    t.last_unmatched = unmatched;
    if unmatched && waited >= UNMATCHED_LOG_AFTER_MS { t.unmatched_once(Unmatched::NoToolUse) } else { None }
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        ticker.tick().await; // skip the immediate first tick

        tracing::info!("subagent prompt watcher started");

        // Keyed by request id, which is never reused, so a prompt whose row was
        // removed simply stops appearing and its entry is pruned.
        let mut tracked: HashMap<u64, Tracked> = HashMap::new();
        loop {
            ticker.tick().await;

            let Some(open) = app.try_state::<AppState>().map(|s| s.pending_subagent_prompts()) else { continue };
            if open.is_empty() {
                tracked.clear();
                continue;
            }
            tracked.retain(|request, _| open.iter().any(|(_, p)| p.request == *request));

            let now = now_ms();
            let mut batch = std::mem::take(&mut tracked);
            let judged = tauri::async_runtime::spawn_blocking(move || {
                let findings: Vec<(String, PendingPrompt, Finding)> = open
                    .into_iter()
                    .filter_map(|(chat_id, p)| {
                        let finding = judge(&p, batch.entry(p.request).or_default(), now)?;
                        Some((chat_id, p, finding))
                    })
                    .collect();
                (batch, findings)
            })
            .await;
            let Ok((kept, findings)) = judged else { continue };
            tracked = kept;

            let Some(state) = app.try_state::<AppState>() else { continue };
            let settled_at = now_ms();
            let mut changed = false;
            for (chat_id, p, finding) in findings {
                match finding {
                    Finding::Settled(m) => {
                        if let Some(o) = state.settle_subagent_prompts(&chat_id, SettleScope::Request(p.request), settled_at) {
                            log_prompt_settled(&chat_id, &o, SettledVia::ToolResult, Some(&m), settled_at);
                            changed = true;
                        }
                    }
                    Finding::Unmatched(why) => tracing::debug!(
                        chat_id = %chat_id,
                        decision = "subagent_prompt_unmatched",
                        request = p.request,
                        agent_id = %p.prompt.agent_id,
                        outcome = why.as_str(),
                        waited_ms = now - p.requested_at,
                        subagents_dir = ?p.prompt.subagents_dir,
                        reason = why.reason(),
                        "subagent prompt unmatched"
                    ),
                }
            }
            if changed {
                emit_sessions_updated(&app);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SubagentPromptRequest;
    use serde_json::json;

    /// The hook's arrival in the fixtures below.
    const T: i64 = 1_790_000_000_000;

    fn stamp(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms).expect("in range").to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// One assistant entry holding one `tool_use` block, as the sidechain writes it.
    fn call(msg: &str, id: &str, name: &str, input: Value, at: i64) -> String {
        json!({
            "type": "assistant", "isSidechain": true, "agentId": "a1", "uuid": format!("u-{id}"), "timestamp": stamp(at),
            "message": {"id": msg, "role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}
        })
        .to_string()
    }

    fn text(msg: &str, body: &str, at: i64) -> String {
        json!({
            "type": "assistant", "isSidechain": true, "agentId": "a1", "uuid": format!("u-{msg}-{at}"), "timestamp": stamp(at),
            "message": {"id": msg, "role": "assistant", "content": [{"type": "text", "text": body}]}
        })
        .to_string()
    }

    fn result(id: &str, is_error: bool, at: i64) -> String {
        json!({
            "type": "user", "isSidechain": true, "agentId": "a1", "uuid": format!("r-{id}"), "timestamp": stamp(at),
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "…", "is_error": is_error}]}
        })
        .to_string()
    }

    fn verdict(lines: &[String], name: &str, input: &Value) -> GateVerdict {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        gate_verdict(&refs, &GateQuery { tool_name: name, tool_input: input, requested_at: T })
    }

    fn settled(ids: &[&str], is_error: bool, matched_on: MatchedOn) -> GateVerdict {
        GateVerdict::Settled(GateMatch { tool_use_ids: ids.iter().map(|s| s.to_string()).collect(), is_error, matched_on })
    }

    #[test]
    fn the_incident_shape_settles_on_the_denial() {
        // The 2026-09-25 shape: the call is on disk ~0.45s before the hook, and
        // the only thing ever written for it is the timeout denial ~120s later.
        let input = json!({"command": "cargo test", "description": "run tests"});
        let mut lines = vec![
            call("msg_0", "toolu_0", "Bash", json!({"command": "ls"}), T - 30_000),
            result("toolu_0", false, T - 29_000),
            text("msg_1", "Running the suite.", T - 500),
            call("msg_1", "toolu_1", "Bash", input.clone(), T - 450),
        ];
        assert_eq!(verdict(&lines, "Bash", &input), GateVerdict::Pending, "the dialog is still open");
        lines.push(result("toolu_1", true, T + 120_280));
        assert_eq!(verdict(&lines, "Bash", &input), settled(&["toolu_1"], true, MatchedOn::Input));
    }

    #[test]
    fn an_identical_call_resolved_in_an_earlier_message_does_not_settle() {
        // The same command ran and resolved 8.2s before the hook; its result
        // must not release the dialog for the repeat.
        let input = json!({"command": "cargo test"});
        let earlier = vec![call("msg_0", "toolu_0", "Bash", input.clone(), T - 9_000), result("toolu_0", false, T - 8_200)];
        assert_eq!(verdict(&earlier, "Bash", &input), GateVerdict::Unmatched, "not flushed yet: nothing to hold on");

        let mut lines = earlier;
        lines.push(call("msg_1", "toolu_1", "Bash", input.clone(), T - 400));
        assert_eq!(verdict(&lines, "Bash", &input), GateVerdict::Pending, "the repeat waits for its own result");
    }

    #[test]
    fn a_fast_approval_written_before_the_hook_arrived_settles() {
        let input = json!({"command": "git status"});
        let lines = vec![call("msg_1", "toolu_1", "Bash", input.clone(), T - 600), result("toolu_1", false, T - 200)];
        assert_eq!(verdict(&lines, "Bash", &input), settled(&["toolu_1"], false, MatchedOn::Input));
    }

    #[test]
    fn a_result_older_than_the_hook_latency_belongs_to_another_call() {
        let input = json!({"command": "git status"});
        let lines = vec![call("msg_1", "toolu_1", "Bash", input.clone(), T - 7_000), result("toolu_1", false, T - 6_000)];
        assert_eq!(verdict(&lines, "Bash", &input), GateVerdict::Unmatched);
    }

    #[test]
    fn parallel_same_name_calls_settle_on_the_input_match() {
        let (a, b) = (json!({"file_path": "a.rs"}), json!({"file_path": "b.rs"}));
        let mut lines = vec![call("msg_1", "toolu_a", "Read", a, T - 500), call("msg_1", "toolu_b", "Read", b.clone(), T - 480), result("toolu_a", false, T - 300)];
        assert_eq!(verdict(&lines, "Read", &b), GateVerdict::Pending, "the sibling's result is not this dialog's");
        lines.push(result("toolu_b", true, T + 120_000));
        assert_eq!(verdict(&lines, "Read", &b), settled(&["toolu_b"], true, MatchedOn::Input));
    }

    #[test]
    fn an_exact_input_match_outranks_a_newer_same_name_call() {
        // Approved fast: `git add` resolved before the tick looked, and the agent's
        // next message — an auto-allowed `cargo test` — was stamped inside the
        // skew window. Recency alone would wait on `cargo test`.
        let add = json!({"command": "git add ."});
        let lines = vec![
            call("msg_1", "toolu_add", "Bash", add.clone(), T - 800),
            result("toolu_add", false, T + 300),
            call("msg_2", "toolu_test", "Bash", json!({"command": "cargo test"}), T + 1_800),
        ];
        assert_eq!(verdict(&lines, "Bash", &add), settled(&["toolu_add"], false, MatchedOn::Input));
    }

    #[test]
    fn without_an_input_match_every_same_name_call_in_the_message_must_resolve() {
        // The hook's input matching nothing verbatim is unverified territory, so
        // every same-name call in the gated message stands in, and all must end.
        let other = json!({"file_path": "c.rs"});
        let mut lines = vec![
            call("msg_1", "toolu_a", "Read", json!({"file_path": "a.rs"}), T - 500),
            call("msg_1", "toolu_b", "Read", json!({"file_path": "b.rs"}), T - 480),
            result("toolu_a", false, T - 300),
        ];
        assert_eq!(verdict(&lines, "Read", &other), GateVerdict::Pending);
        lines.push(result("toolu_b", true, T + 120_000));
        assert_eq!(verdict(&lines, "Read", &other), settled(&["toolu_a", "toolu_b"], true, MatchedOn::Name));
    }

    #[test]
    fn a_newer_message_without_the_tool_does_not_hide_the_gated_one() {
        let input = json!({"command": "npm test"});
        let mut lines = vec![
            call("msg_1", "toolu_1", "Bash", input.clone(), T - 500),
            text("msg_2", "Meanwhile, reading the config.", T + 300),
            call("msg_2", "toolu_2", "Read", json!({"file_path": "x"}), T + 350),
            result("toolu_2", false, T + 400),
        ];
        assert_eq!(verdict(&lines, "Bash", &input), GateVerdict::Pending);
        lines.push(result("toolu_1", false, T + 20_000));
        assert_eq!(verdict(&lines, "Bash", &input), settled(&["toolu_1"], false, MatchedOn::Input));
    }

    #[test]
    fn results_written_out_of_timestamp_order_still_match() {
        // A sibling's result lands before the next `tool_use` of the same
        // message, and the two results are written against their stamp order.
        let input = json!({"command": "b"});
        let lines = vec![
            call("msg_1", "toolu_a", "Bash", json!({"command": "a"}), T - 600),
            result("toolu_a", false, T + 9_000),
            call("msg_1", "toolu_b", "Bash", input.clone(), T - 550),
            result("toolu_b", true, T + 8_000),
        ];
        assert_eq!(verdict(&lines, "Bash", &input), settled(&["toolu_b"], true, MatchedOn::Input));
    }

    #[test]
    fn malformed_and_partial_lines_are_skipped() {
        let input = json!({"command": "make"});
        let unstamped = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": false}]}}).to_string();
        let mut lines = vec![
            "not json at all".to_string(),
            call("msg_1", "toolu_1", "Bash", input.clone(), T - 500),
            unstamped,
            "{\"type\":\"user\",\"timestamp\":\"2026".to_string(),
        ];
        assert_eq!(verdict(&lines, "Bash", &input), GateVerdict::Pending, "an unstamped result cannot be placed against the hook");
        lines.push(result("toolu_1", false, T + 3_000));
        assert_eq!(verdict(&lines, "Bash", &input), settled(&["toolu_1"], false, MatchedOn::Input));
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccd-subagent-gate-{}-{tag}-{}", std::process::id(), std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_agent_transcript_reaches_a_workflow_subdir() {
        let dir = scratch_dir("find").join("subagents");
        let nested = dir.join("workflows").join("wf_abc-123");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("agent-a1.jsonl"), "").unwrap();
        std::fs::write(dir.join("agent-a2.jsonl"), "").unwrap();
        assert_eq!(find_agent_transcript(&dir, "a1"), Some(nested.join("agent-a1.jsonl")));
        assert_eq!(find_agent_transcript(&dir, "a2"), Some(dir.join("agent-a2.jsonl")));
        assert_eq!(find_agent_transcript(&dir, "a3"), None);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    fn open_prompt(dir: Option<PathBuf>, input: Value) -> PendingPrompt {
        let prompt = SubagentPromptRequest { agent_id: "a1".into(), session_id: "sess".into(), agent_type: Some("reviewer".into()), tool_name: "Bash".into(), tool_input: input, label: "needs approval: Bash".into(), subagents_dir: dir };
        PendingPrompt { request: 1, requested_at: T, prompt }
    }

    #[test]
    fn the_tick_rereads_a_grown_transcript_and_reports_unmatched_once() {
        let dir = scratch_dir("judge").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("agent-a1.jsonl");
        let input = json!({"command": "make"});
        let p = open_prompt(Some(dir.clone()), input.clone());
        let mut t = Tracked::default();

        assert_eq!(judge(&p, &mut t, T + 500), None, "too early to judge");
        assert_eq!(judge(&p, &mut t, T + 2_000), None, "no transcript yet, and not yet worth a line");
        assert_eq!(judge(&p, &mut t, T + 11_000), Some(Finding::Unmatched(Unmatched::NoTranscript)));

        std::fs::write(&file, format!("{}\n", text("msg_0", "Thinking.", T - 900))).unwrap();
        assert_eq!(judge(&p, &mut t, T + 12_000), None, "a second unmatched outcome is never logged");
        assert!(t.last_unmatched);

        std::fs::write(&file, format!("{}\n{}\n", call("msg_1", "toolu_1", "Bash", input.clone(), T - 400), result("toolu_1", true, T + 120_000))).unwrap();
        assert_eq!(judge(&p, &mut t, T + 121_000), Some(Finding::Settled(GateMatch { tool_use_ids: vec!["toolu_1".into()], is_error: true, matched_on: MatchedOn::Input })));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_prompt_without_a_transcript_path_is_reported_once() {
        let p = open_prompt(None, json!({}));
        let mut t = Tracked::default();
        assert_eq!(judge(&p, &mut t, T + 1_000), Some(Finding::Unmatched(Unmatched::NoTranscriptPath)));
        assert_eq!(judge(&p, &mut t, T + 3_000), None);
    }
}
