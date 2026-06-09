//! Onboarding helpers: ship the Python hook script alongside the binary, and
//! produce a ready-to-paste `~/.claude/settings.json` snippet whose `command`
//! string already points at the deployed copy. Users no longer need to clone
//! the repo to wire Claude Code into the dashboard.

use std::path::{Path, PathBuf};

/// The Python hook source, embedded into the binary at compile time. Written
/// to `app_data_dir/claude_hook.py` on startup so users can paste a working
/// `command` into `~/.claude/settings.json` without cloning the repo.
pub const HOOK_SCRIPT: &str = include_str!("../../integrations/claude_hook.py");

pub const HOOK_SCRIPT_FILENAME: &str = "claude_hook.py";

/// Write the embedded hook script to `<app_data>/claude_hook.py`, overwriting
/// any existing copy so app updates keep the on-disk script in sync with the
/// binary. Returns the resolved path on success.
pub fn write_hook_script(app_data: &Path) -> std::io::Result<PathBuf> {
    let path = app_data.join(HOOK_SCRIPT_FILENAME);
    std::fs::write(&path, HOOK_SCRIPT)?;
    Ok(path)
}

/// Python launcher name to use in the generated snippet's `command` strings.
/// `python` on Windows (where `python3` is rarely on PATH outside of pyenv),
/// `python3` elsewhere (macOS / Linux drop the `python` alias post-Python 2).
#[cfg(target_os = "windows")]
pub const PYTHON_CMD: &str = "python";
#[cfg(not(target_os = "windows"))]
pub const PYTHON_CMD: &str = "python3";

/// Convert a filesystem path to a forward-slash form suitable for embedding
/// in JSON. Python on Windows accepts forward slashes, so we avoid the
/// double-backslash escaping that would otherwise be needed.
pub fn path_for_snippet(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Produce the `~/.claude/settings.json` snippet to display in the onboarding
/// panel. The path is wrapped in `\"...\"` so paths containing spaces (e.g.
/// `C:/Users/Some User/AppData/...`) still parse as one argument.
pub fn build_settings_snippet(hook_path_for_command: &str) -> String {
    let cmd = format!("{PYTHON_CMD} \\\"{hook_path_for_command}\\\"");
    format!(
        r#"{{
  "hooks": {{
    "SessionStart":        [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "UserPromptSubmit":    [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "UserPromptExpansion": [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "Notification":        [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "Stop":                [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "StopFailure":         [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "PermissionRequest":   [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "Elicitation":         [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "PreCompact":          [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "SessionEnd":          [{{"hooks": [{{"type": "command", "command": "{cmd}"}}]}}],
    "PreToolUse": [{{
      "matcher": "^(AskUserQuestion|ExitPlanMode)$",
      "hooks": [{{"type": "command", "command": "{cmd}"}}]
    }}]
  }}
}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_contains_path_and_python_command() {
        let snippet = build_settings_snippet("C:/Users/x/AppData/Roaming/app/claude_hook.py");
        assert!(snippet.contains("C:/Users/x/AppData/Roaming/app/claude_hook.py"));
        assert!(snippet.contains(PYTHON_CMD));
        assert!(snippet.contains("AskUserQuestion|ExitPlanMode"));
    }

    #[test]
    fn snippet_quotes_path_for_spaces() {
        let snippet = build_settings_snippet("/Users/Some User/Library/x/claude_hook.py");
        // The path appears wrapped in escaped quotes so JSON parses one
        // argument even when the path has spaces.
        assert!(snippet.contains(r#"\"/Users/Some User/Library/x/claude_hook.py\""#));
    }

    #[test]
    fn path_for_snippet_normalizes_backslashes() {
        let path = Path::new(r"C:\Users\x\AppData\claude_hook.py");
        assert_eq!(
            path_for_snippet(path),
            "C:/Users/x/AppData/claude_hook.py"
        );
    }

    #[test]
    fn write_hook_script_writes_embedded_source() {
        let dir = std::env::temp_dir().join(format!("setup_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_hook_script(&dir).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, HOOK_SCRIPT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
