//! Harvests token usage out of Claude Code's transcripts into
//! [`crate::token_history`].
//!
//! Deliberately a standalone tree scan rather than an extension of
//! [`crate::log_watcher`]. That watcher tails exactly one hook-supplied
//! `transcript_path` per chat_id, so `<session>/subagents/**.jsonl` is never
//! opened — 616 of 660 files in a real tree, carrying 55.5% of all output
//! tokens. Its extraction is also first-wins and sidechain-blind on purpose
//! (it feeds the context gauge, which wants the main turn's context size, not a
//! sum), and it never reads `output_tokens` at all. A tree scan additionally
//! covers sessions the dashboard never tracked and catches up on work done
//! while the app was closed.
//!
//! Progress is a per-file byte cursor, so a steady-state pass costs one `stat`
//! per file plus a read of whatever was appended. Nothing here needs to be
//! careful about double-counting: [`crate::token_history::reduce_by_id`] makes a
//! re-read of an already-consumed file a no-op, which is why a truncated or
//! rotated file can simply be re-read from zero.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::token_history::{TokenHistoryStore, TokenRecord};

/// Rescan cadence. Transcripts are appended continuously, but the chart bins to
/// 10 minutes, so a minute of lag is invisible.
const POLL: Duration = Duration::from_secs(60);

/// Cap on records appended in one pass, so the first (historical) pass can't
/// block the runtime for an unbounded time. The cursor makes the remainder
/// simply arrive on the next tick.
const MAX_RECORDS_PER_PASS: usize = 20_000;

/// How far each transcript has been consumed, keyed by absolute path.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScanCursor {
    #[serde(default)]
    consumed: HashMap<String, u64>,
}

/// Claude Code's config directory: `$CLAUDE_CONFIG_DIR` when set, else
/// `~/.claude`. The env var is the documented override (it appears in the
/// 2.1.238 binary) but a GUI app on macOS inherits no shell profile, so the
/// home-relative form is the path that actually resolves in production — hence
/// the scan logs the root it resolved on every pass rather than assuming it.
/// Shared with `session_registry`, which reads `sessions/` under the same root.
pub(crate) fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs_home().map(|h| h.join(".claude"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

pub fn projects_root() -> Option<PathBuf> {
    config_dir().map(|d| d.join("projects"))
}

/// Every `*.jsonl` under `root`, at any depth. Subagent transcripts live in a
/// nested `subagents/` directory, so this cannot be a flat read_dir.
fn transcript_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "jsonl") => out.push(path),
                _ => {}
            }
        }
    }
    out
}

/// Extract one transcript line's token usage, or `None` when the line carries
/// none (user turns, tool results, summaries, malformed JSON).
///
/// `seq` is left at 0 — [`TokenHistoryStore::append_batch`] assigns the real
/// one. Pure, so the parsing rules are testable without a transcript on disk.
pub fn parse_usage_line(line: &str) -> Option<TokenRecord> {
    if !line.contains("\"usage\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let id = message.get("id")?.as_str()?;
    let ts = value.get("timestamp")?.as_str().and_then(parse_rfc3339_ms)?;
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    let record = TokenRecord {
        ts,
        id: id.to_string(),
        seq: 0,
        input: field("input_tokens"),
        cache_creation: field("cache_creation_input_tokens"),
        cache_read: field("cache_read_input_tokens"),
        output: field("output_tokens"),
    };
    // A usage object with every component zero carries no work; dropping it here
    // keeps the store free of rows that can only dilute a bucket count.
    if record.work_tokens() == 0 && record.cache_read == 0 {
        return None;
    }
    Some(record)
}

/// Parse the RFC3339 timestamps Claude Code writes (always UTC, `...Z`) to ms
/// since epoch.
fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|dt| dt.timestamp_millis())
}

/// Read the bytes appended to `path` since `consumed`, returning the parsed
/// records and the new cursor position.
///
/// A file shorter than its cursor was rotated or truncated, so it is re-read
/// from zero — safe because the id reduction makes re-reading idempotent. The
/// cursor advances only to the last complete line, so a partially-flushed
/// trailing line is re-read next pass instead of being lost.
fn scan_file(path: &Path, consumed: u64) -> Option<(Vec<TokenRecord>, u64)> {
    let len = std::fs::metadata(path).ok()?.len();
    let start = if len < consumed { 0 } else { consumed };
    if len == start {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let bytes = contents.as_bytes();
    let from = if start as usize > bytes.len() { 0 } else { start as usize };
    let tail = &contents[from..];
    let complete = tail.rfind('\n').map(|i| i + 1).unwrap_or(0);
    if complete == 0 {
        return None;
    }
    let records: Vec<TokenRecord> = tail[..complete].lines().filter_map(parse_usage_line).collect();
    Some((records, from as u64 + complete as u64))
}

/// One read-only pass over the tree: prune cursors for rotated files, then
/// parse whatever each transcript has gained. Returns the file count, the
/// parsed records, and the cursor positions those records justify.
///
/// Pure file work with no app state, so it runs on a blocking thread; the
/// caller owns the append and only then commits `positions`, which keeps a
/// failed write retryable instead of silently skipped.
fn collect_once(root: &Path, cursor: &mut ScanCursor) -> (usize, Vec<TokenRecord>, Vec<(String, u64)>) {
    let files = transcript_files(root);
    let seen: std::collections::HashSet<String> = files.iter().map(|p| p.display().to_string()).collect();
    // Drop cursors for transcripts Claude Code has rotated away, so the map
    // doesn't grow without bound across the ~30-day retention window.
    cursor.consumed.retain(|path, _| seen.contains(path));

    let mut batch: Vec<TokenRecord> = Vec::new();
    let mut positions: Vec<(String, u64)> = Vec::new();
    for path in files.iter() {
        if batch.len() >= MAX_RECORDS_PER_PASS {
            break;
        }
        let key = path.display().to_string();
        let consumed = cursor.consumed.get(&key).copied().unwrap_or(0);
        let Some((records, next)) = scan_file(path, consumed) else { continue };
        batch.extend(records);
        positions.push((key, next));
    }
    (files.len(), batch, positions)
}

/// Apply the positions a successful append has earned.
fn commit(cursor: &mut ScanCursor, positions: Vec<(String, u64)>) {
    for (key, next) in positions {
        cursor.consumed.insert(key, next);
    }
}

fn cursor_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("token_scan_cursor.json"))
}

fn load_cursor(path: &Path) -> ScanCursor {
    std::fs::read_to_string(path).ok().and_then(|c| serde_json::from_str(&c).ok()).unwrap_or_default()
}

fn save_cursor(path: &Path, cursor: &ScanCursor) {
    let Ok(json) = serde_json::to_string(cursor) else { return };
    if let Err(e) = std::fs::write(path, json) {
        tracing::warn!(?e, path = %path.display(), "failed to save token scan cursor");
    }
}

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(root) = projects_root() else {
            tracing::warn!("token scan disabled: no home directory resolved");
            return;
        };
        let Some(cursor_file) = cursor_path(&app) else { return };
        tracing::info!(root = %root.display(), "token scanner started");

        let mut ticker = tokio::time::interval(POLL);
        loop {
            ticker.tick().await;

            let scan_root = root.clone();
            let scan_cursor_file = cursor_file.clone();
            // Blocking file work off the async runtime: the first pass parses the
            // whole tree and must not stall the tray or the widget. Only the
            // reading happens here — the store is managed state and stays on this
            // side, so it remains the single writer.
            let scanned = tauri::async_runtime::spawn_blocking(move || {
                let started = std::time::Instant::now();
                let mut cursor = load_cursor(&scan_cursor_file);
                let (files, batch, positions) = collect_once(&scan_root, &mut cursor);
                (files, batch, positions, cursor, started.elapsed().as_millis() as u64)
            })
            .await;

            let Ok((files, batch, positions, mut cursor, took_ms)) = scanned else { continue };
            let mut appended = 0;
            if batch.is_empty() {
                // No new records, but the prune above still needs persisting.
                save_cursor(&cursor_file, &cursor);
            } else if let Some(store) = app.try_state::<TokenHistoryStore>() {
                if store.append_batch(&batch).is_some() {
                    appended = batch.len();
                    commit(&mut cursor, positions);
                    save_cursor(&cursor_file, &cursor);
                }
            }
            if files == 0 {
                // Not silently nothing: a zero-file pass means the resolved root
                // is wrong, which otherwise looks identical to "no work done".
                tracing::warn!(root = %root.display(), decision = "token_scan", reason = "no transcripts found at the resolved root", "token scan");
            } else if appended > 0 {
                tracing::debug!(files, appended, took_ms, decision = "token_scan", reason = "appended token records", "token scan");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"timestamp":"2026-08-01T10:00:00.000Z","message":{"id":"msg_1","usage":{"input_tokens":5,"cache_creation_input_tokens":40,"cache_read_input_tokens":900,"output_tokens":120}}}"#;

    #[test]
    fn parses_a_real_assistant_line() {
        let r = parse_usage_line(LINE).expect("should parse");
        assert_eq!(r.id, "msg_1");
        assert_eq!((r.input, r.cache_creation, r.cache_read, r.output), (5, 40, 900, 120));
        assert_eq!(r.work_tokens(), 165);
        assert_eq!(r.ts, 1785578400000); // 2026-08-01T10:00:00Z
    }

    #[test]
    fn skips_lines_without_usage() {
        assert!(parse_usage_line(r#"{"type":"user","message":{"content":"hi"}}"#).is_none());
        assert!(parse_usage_line("not json at all").is_none());
        assert!(parse_usage_line("").is_none());
    }

    #[test]
    fn skips_a_line_missing_an_id_or_timestamp() {
        // Both are required: the id is the dedup key, the timestamp the bucket.
        assert!(parse_usage_line(r#"{"timestamp":"2026-08-01T10:00:00.000Z","message":{"usage":{"output_tokens":5}}}"#).is_none());
        assert!(parse_usage_line(r#"{"message":{"id":"m","usage":{"output_tokens":5}}}"#).is_none());
    }

    #[test]
    fn skips_an_all_zero_usage_object() {
        let line = r#"{"timestamp":"2026-08-01T10:00:00.000Z","message":{"id":"m","usage":{"input_tokens":0,"output_tokens":0}}}"#;
        assert!(parse_usage_line(line).is_none());
    }

    #[test]
    fn missing_components_default_to_zero() {
        let line = r#"{"timestamp":"2026-08-01T10:00:00.000Z","message":{"id":"m","usage":{"output_tokens":7}}}"#;
        let r = parse_usage_line(line).expect("should parse");
        assert_eq!((r.input, r.cache_creation, r.cache_read, r.output), (0, 0, 0, 7));
    }

    fn temp_file(name: &str, body: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("token_scan_{}_{}_{}.jsonl", std::process::id(), name, std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn scan_file_reads_only_the_appended_tail() {
        let path = temp_file("tail", &format!("{LINE}\n"));
        let (first, pos) = scan_file(&path, 0).expect("first pass");
        assert_eq!(first.len(), 1);

        let second = scan_file(&path, pos);
        assert!(second.is_none(), "an unchanged file yields nothing");

        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str(&LINE.replace("msg_1", "msg_2"));
        body.push('\n');
        std::fs::write(&path, &body).unwrap();
        let (third, _) = scan_file(&path, pos).expect("appended lines");
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].id, "msg_2");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_file_ignores_a_partially_written_trailing_line() {
        // A transcript caught mid-flush: the cursor must stop at the last
        // newline so the incomplete line is read whole on the next pass.
        let path = temp_file("partial", &format!("{LINE}\n{{\"timestamp\":\"2026"));
        let (records, pos) = scan_file(&path, 0).expect("complete lines only");
        assert_eq!(records.len(), 1);
        assert_eq!(pos as usize, LINE.len() + 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_file_rereads_a_truncated_file_from_the_start() {
        // Rotation shrinks the file below the cursor. Re-reading is safe because
        // reduce_by_id makes duplicate ids idempotent.
        let path = temp_file("rotated", &format!("{LINE}\n"));
        let (records, _) = scan_file(&path, 9_999).expect("re-read from zero");
        assert_eq!(records.len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
