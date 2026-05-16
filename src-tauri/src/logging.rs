use std::io::Write;
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{fmt, EnvFilter};

/// Holder struct for the appender's worker guard. Keeping this in managed
/// state ensures the background writer thread is flushed and joined at
/// shutdown (the guard's Drop blocks until pending log lines are written).
pub struct LogGuard(#[allow(dead_code)] WorkerGuard);

/// Direct-to-file JSONL writer for frontend events. Bypasses `tracing`
/// because tracing's macros require static field names and the default JSON
/// formatter renders nested values as escaped strings — neither fits a
/// generic key/value payload arriving over IPC. The output envelope mirrors
/// `tracing-subscriber`'s `Json` formatter so frontend and backend lines
/// interleave cleanly in widget.jsonl.
pub struct FrontendLogger {
    writer: NonBlocking,
}

#[derive(Serialize)]
struct FrontendLogLine<'a> {
    timestamp: String,
    level: &'a str,
    fields: Value,
    target: &'a str,
}

impl FrontendLogger {
    pub fn log(&self, level: &str, message: &str, data: Value) {
        let mut fields = serde_json::Map::new();
        fields.insert("message".to_string(), Value::String(message.to_string()));
        if let Value::Object(map) = data {
            for (k, v) in map {
                if k == "message" {
                    continue;
                }
                fields.insert(k, v);
            }
        }
        let normalized_level = match level.to_ascii_lowercase().as_str() {
            "error" => "ERROR",
            "warn" => "WARN",
            "info" => "INFO",
            "trace" => "TRACE",
            _ => "DEBUG",
        };
        let line = FrontendLogLine {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
            level: normalized_level,
            fields: Value::Object(fields),
            target: "claude_code_dashboard_lib::frontend",
        };
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        let mut w = self.writer.clone();
        let _ = w.write_all(&bytes);
    }
}

pub fn init(log_dir: &Path) -> (LogGuard, FrontendLogger) {
    std::fs::create_dir_all(log_dir).ok();
    let appender = tracing_appender::rolling::never(log_dir, "widget.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,claude_code_dashboard_lib=debug"));

    let frontend = FrontendLogger {
        writer: writer.clone(),
    };

    fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .ok();

    (LogGuard(guard), frontend)
}
